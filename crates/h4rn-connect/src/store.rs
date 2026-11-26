//! Postgres persistence (ADR-012).
//!
//! All queries are runtime-checked (`sqlx::query_as` with `FromRow`) rather
//! than compile-time-checked macros. `sqlx-cli` is not installed in this
//! environment, so there is no offline metadata to check in; the schema is
//! applied from `migrations/` at boot and every query is exercised by the
//! Docker-backed integration tests.

use chrono::{DateTime, Duration, Utc};
use h4rn_protocol::{
    ConnectDevice, ConnectDeviceKind, ConnectSessionTokens, ConnectUser, CostSource, MachineHost,
    MachineHostKind, MachineSummary, PairScope, PairingState, PresenceUpdate, UsageBucket,
    UsageBucketPoint, UsageEvent, UsageGroup, UsageGroupBy, UsageProvenance, UsageReport,
    UsageScope, UsageSyncResponse, UsageSyncRow, UsageTotals,
};
use sqlx::Row as _;
use sqlx::postgres::PgPoolOptions;
use sqlx::{FromRow, PgPool, Postgres, Transaction};
use uuid::Uuid;

use crate::config::Config;
use crate::crypto::{
    Envelope, Keystore, SealedSecret, b64, generate_short_code, hash_pairing_code,
    hash_refresh_token, random_b64, verify_pairing_secret,
};
use crate::error::{AppError, Result};
use crate::jwt::{AccessClaims, issue_access_token};

/// Columns returned when reading an envelope-encrypted secret.
type SecretColumns = (Vec<u8>, Vec<u8>, Vec<u8>, Vec<u8>);

/// A user row.
#[derive(Clone, Debug, FromRow)]
pub struct UserRow {
    /// User id.
    pub id: Uuid,
    /// Primary email.
    pub email: Option<String>,
    /// Display name.
    pub display_name: Option<String>,
    /// Avatar URL.
    pub avatar_url: Option<String>,
    /// Creation time.
    pub created_at: DateTime<Utc>,
}

/// An identity row.
#[derive(Clone, Debug, FromRow)]
pub struct IdentityRow {
    /// Identity id.
    pub id: Uuid,
    /// Owner.
    pub user_id: Uuid,
    /// Configured provider id.
    pub provider_id: String,
    /// Issuer.
    pub issuer: String,
    /// Subject.
    pub subject: String,
    /// Email at last login.
    pub email: Option<String>,
    /// Verified flag.
    pub email_verified: bool,
}

/// A device row.
#[derive(Clone, Debug, FromRow)]
pub struct DeviceRow {
    /// Device id.
    pub id: Uuid,
    /// Owner.
    pub user_id: Uuid,
    /// Machine for a daemon device.
    pub machine_id: Option<Uuid>,
    /// `daemon|desktop|mobile|web`.
    pub kind: String,
    /// Display name.
    pub name: String,
    /// Ed25519 proof key.
    pub proof_public_key: Vec<u8>,
    /// X25519 Noise static key.
    pub noise_static_public_key: Option<Vec<u8>>,
    /// Platform.
    pub platform: Option<String>,
    /// Last seen.
    pub last_seen: Option<DateTime<Utc>>,
    /// Revocation time.
    pub revoked_at: Option<DateTime<Utc>>,
    /// Creation time.
    pub created_at: DateTime<Utc>,
}

impl DeviceRow {
    /// The wire representation.
    pub fn to_wire(&self) -> ConnectDevice {
        ConnectDevice {
            id: self.id.to_string(),
            kind: ConnectDeviceKind::parse(&self.kind).unwrap_or(ConnectDeviceKind::Web),
            name: self.name.clone(),
            machine_id: self.machine_id.map(|id| id.to_string()),
            proof_public_key: b64(&self.proof_public_key),
            noise_static_public_key: self.noise_static_public_key.as_deref().map(b64),
            platform: self.platform.clone(),
            created_at: self.created_at,
            last_seen: self.last_seen,
            revoked_at: self.revoked_at,
            scopes: Vec::new(),
        }
    }
}

/// One phone registered for push alerts (ADR-051, ADR-052).
#[derive(Clone, Debug, FromRow)]
pub struct PushDeviceRow {
    /// Row id.
    pub id: Uuid,
    /// Owner.
    pub user_id: Uuid,
    /// h4rn-connect device id this install signed in as.
    pub device_id: String,
    /// Push token: an FCM registration token on Android, an APNs device token
    /// on iOS.
    pub token: String,
    /// `android` or `ios`.
    pub platform: String,
    /// Android package name, so FCM can restrict the message to it.
    pub package_name: Option<String>,
    /// App version, for diagnostics.
    pub app_version: Option<String>,
    /// Creation time.
    pub created_at: DateTime<Utc>,
    /// Last registration or rotation.
    pub updated_at: DateTime<Utc>,
}

/// A machine row.
#[derive(Clone, Debug, FromRow)]
pub struct MachineRow {
    /// Machine id.
    pub id: Uuid,
    /// Owner.
    pub user_id: Uuid,
    /// Display name.
    pub name: String,
    /// Owning daemon device.
    pub daemon_device_id: Option<Uuid>,
    /// Creation time.
    pub created_at: DateTime<Utc>,
    /// Update time.
    pub updated_at: DateTime<Utc>,
}

/// A host row.
#[derive(Clone, Debug, FromRow)]
pub struct HostRow {
    /// Host id.
    pub id: Uuid,
    /// Machine.
    pub machine_id: Uuid,
    /// Display name.
    pub name: String,
    /// `host|wsl`.
    pub kind: String,
    /// WSL distro.
    pub distro: Option<String>,
}

impl HostRow {
    /// The wire representation.
    pub fn to_wire(&self) -> MachineHost {
        MachineHost {
            id: self.id.to_string(),
            name: self.name.clone(),
            kind: if self.kind == "wsl" {
                MachineHostKind::Wsl
            } else {
                MachineHostKind::Host
            },
            distro: self.distro.clone(),
        }
    }
}

/// A session row.
#[derive(Clone, Debug, FromRow)]
pub struct SessionRow {
    /// Session id.
    pub id: Uuid,
    /// Refresh family.
    pub family_id: Uuid,
    /// Owner.
    pub user_id: Uuid,
    /// Device.
    pub device_id: Uuid,
    /// Refresh-token hash.
    pub refresh_hash: String,
    /// Access-token id.
    pub access_jti: Uuid,
    /// Issued at.
    pub issued_at: DateTime<Utc>,
    /// Refresh expiry.
    pub expires_at: DateTime<Utc>,
    /// Rotation time.
    pub rotated_at: Option<DateTime<Utc>>,
    /// Successor row.
    pub replaced_by: Option<Uuid>,
    /// Encrypted successor refresh token for the grace window.
    pub successor_secret: Option<Vec<u8>>,
    /// Revocation time.
    pub revoked_at: Option<DateTime<Utc>>,
    /// Reason.
    pub revoke_reason: Option<String>,
}

/// A pairing row.
#[derive(Clone, Debug, FromRow)]
pub struct PairingRow {
    /// Pairing id.
    pub id: Uuid,
    /// Machine.
    pub machine_id: Uuid,
    /// Creating device.
    pub created_by_device_id: Uuid,
    /// Secret hash.
    pub secret_hash: String,
    /// State.
    pub state: String,
    /// Expiry.
    pub expires_at: DateTime<Utc>,
    /// Claiming device.
    pub claimed_by_device_id: Option<Uuid>,
    /// Claim time.
    pub claimed_at: Option<DateTime<Utc>>,
    /// Failed attempts.
    pub attempts: i32,
    /// Creation time.
    pub created_at: DateTime<Utc>,
    /// Scopes the claiming device will hold.
    pub scopes: Vec<String>,
}

impl PairingRow {
    /// The wire representation, computing expiry lazily.
    pub fn to_wire(&self, now: DateTime<Utc>) -> h4rn_protocol::PairingInfo {
        let state = if self.state == "pending" && self.expires_at <= now {
            PairingState::Expired
        } else {
            match self.state.as_str() {
                "claimed" => PairingState::Claimed,
                "revoked" => PairingState::Revoked,
                "expired" => PairingState::Expired,
                _ => PairingState::Pending,
            }
        };
        h4rn_protocol::PairingInfo {
            id: self.id.to_string(),
            machine_id: self.machine_id.to_string(),
            state,
            created_at: self.created_at,
            expires_at: self.expires_at,
            claimed_by_device_id: self.claimed_by_device_id.map(|id| id.to_string()),
            scopes: parse_scopes(&self.scopes),
        }
    }
}

/// Parses stored scope spellings, dropping any this build does not know.
pub fn parse_scopes(values: &[String]) -> Vec<PairScope> {
    PairScope::normalize(values.iter().filter_map(|value| PairScope::parse(value)))
}

fn scope_strings(scopes: &[PairScope]) -> Vec<String> {
    scopes
        .iter()
        .map(|scope| scope.as_str().to_owned())
        .collect()
}

/// An OIDC login flow.
#[derive(Clone, Debug, FromRow)]
pub struct AuthFlowRow {
    /// Flow id.
    pub id: Uuid,
    /// Provider.
    pub provider_id: String,
    /// State.
    pub state: String,
    /// Nonce.
    pub nonce: String,
    /// PKCE verifier.
    pub pkce_verifier: String,
    /// Redirect URI.
    pub redirect_uri: String,
    /// Device kind.
    pub device_kind: String,
    /// Device name.
    pub device_name: String,
    /// Proof key.
    pub proof_public_key: Vec<u8>,
    /// Noise key.
    pub noise_static_public_key: Option<Vec<u8>>,
    /// Platform.
    pub platform: Option<String>,
    /// Machine name for daemons.
    pub machine_name: Option<String>,
    /// Expiry.
    pub expires_at: DateTime<Utc>,
}

/// A device authorization grant.
#[derive(Clone, Debug, FromRow)]
pub struct DeviceGrantRow {
    /// Grant id.
    pub id: Uuid,
    /// Provider.
    pub provider_id: String,
    /// Provider device code.
    pub device_code: String,
    /// User code.
    pub user_code: String,
    /// Verification URI.
    pub verification_uri: String,
    /// Verification URI with the code.
    pub verification_uri_complete: Option<String>,
    /// Poll interval.
    pub interval_secs: i32,
    /// Expiry.
    pub expires_at: DateTime<Utc>,
    /// State.
    pub status: String,
    /// Device kind.
    pub device_kind: String,
    /// Device name.
    pub device_name: String,
    /// Proof key.
    pub proof_public_key: Vec<u8>,
    /// Noise key.
    pub noise_static_public_key: Option<Vec<u8>>,
    /// Platform.
    pub platform: Option<String>,
    /// Machine name.
    pub machine_name: Option<String>,
    /// Owner, after approval.
    pub user_id: Option<Uuid>,
    /// Device, after approval.
    pub device_id: Option<Uuid>,
    /// Session issued when the grant was approved.
    pub session_id: Option<Uuid>,
    /// Envelope-encrypted refresh token for idempotent polls.
    pub refresh_ciphertext: Option<Vec<u8>>,
    /// Last poll.
    pub last_polled_at: Option<DateTime<Utc>>,
}

/// An audit row.
#[derive(Clone, Debug, FromRow)]
pub struct AuditRow {
    /// Id/cursor.
    pub id: i64,
    /// Acting device.
    pub actor_device_id: Option<Uuid>,
    /// Action.
    pub action: String,
    /// Target type.
    pub target_type: Option<String>,
    /// Target id.
    pub target_id: Option<String>,
    /// Metadata.
    pub metadata: serde_json::Value,
    /// Time.
    pub at: DateTime<Utc>,
}

/// Device registration input.
#[derive(Clone, Debug)]
pub struct NewDevice {
    /// Kind.
    pub kind: ConnectDeviceKind,
    /// Name.
    pub name: String,
    /// Ed25519 proof key.
    pub proof_public_key: Vec<u8>,
    /// X25519 Noise key.
    pub noise_static_public_key: Option<Vec<u8>>,
    /// Platform.
    pub platform: Option<String>,
    /// Machine name for daemons.
    pub machine_name: Option<String>,
}

/// Inputs for storing a new device authorization grant.
#[derive(Clone, Copy, Debug)]
pub struct NewDeviceGrant<'a> {
    /// Provider id.
    pub provider_id: &'a str,
    /// Provider device code.
    pub device_code: &'a str,
    /// User-facing code.
    pub user_code: &'a str,
    /// Verification URI.
    pub verification_uri: &'a str,
    /// Verification URI including the code.
    pub verification_uri_complete: Option<&'a str>,
    /// Poll interval.
    pub interval_secs: i32,
    /// Lifetime in seconds.
    pub expires_in: i64,
    /// Device registration.
    pub device: &'a NewDevice,
}

/// Result of a login/device-grant approval.
#[derive(Clone, Debug)]
pub struct Authenticated {
    /// User row.
    pub user: UserRow,
    /// Device row.
    pub device: DeviceRow,
    /// Session row.
    pub session: SessionRow,
}

/// Issued tokens plus their session row.
#[derive(Clone, Debug)]
pub struct IssuedSession {
    /// Tokens.
    pub tokens: ConnectSessionTokens,
    /// Session row.
    pub session: SessionRow,
}

/// Rotation outcome before touching the database.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReuseDecision {
    /// The token is fresh and should rotate.
    Rotate,
    /// The token was rotated inside the grace window; reissue its successor.
    ReplaySuccessor,
    /// The token was reused outside the window; revoke the family.
    RevokeFamily,
}

/// Classifies a refresh-token presentation.
pub fn classify_reuse(
    rotated_at: Option<DateTime<Utc>>,
    now: DateTime<Utc>,
    grace: Duration,
) -> ReuseDecision {
    match rotated_at {
        None => ReuseDecision::Rotate,
        Some(rotated_at) if now - rotated_at <= grace => ReuseDecision::ReplaySuccessor,
        Some(_) => ReuseDecision::RevokeFamily,
    }
}

/// The relay's Postgres handle.
#[derive(Clone, Debug)]
pub struct Store {
    pool: PgPool,
}

impl Store {
    /// Connects and applies migrations.
    pub async fn connect(config: &Config) -> Result<Self> {
        let pool = PgPoolOptions::new()
            .max_connections(16)
            .acquire_timeout(std::time::Duration::from_secs(10))
            .connect(&config.database_url)
            .await
            .map_err(|error| {
                AppError::unavailable(
                    "database_unavailable",
                    format!("could not connect to Postgres: {error}"),
                )
            })?;
        let store = Self { pool };
        store.migrate().await?;
        Ok(store)
    }

    /// Builds a store from an existing pool (tests).
    pub fn from_pool(pool: PgPool) -> Self {
        Self { pool }
    }

    /// The underlying pool.
    pub fn pool(&self) -> &PgPool {
        &self.pool
    }

    /// Applies embedded migrations.
    pub async fn migrate(&self) -> Result<()> {
        sqlx::migrate!("./migrations")
            .run(&self.pool)
            .await
            .map_err(|error| AppError::internal(format!("migration failed: {error}")))
    }

    /// Liveness of the database.
    pub async fn ping(&self) -> bool {
        sqlx::query("SELECT 1").execute(&self.pool).await.is_ok()
    }

    /// Finds a user.
    pub async fn user(&self, id: Uuid) -> Result<UserRow> {
        sqlx::query_as::<_, UserRow>(
            "SELECT id, email, display_name, avatar_url, created_at FROM users WHERE id = $1",
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await?
        .ok_or_else(|| AppError::not_found("user_not_found", "user does not exist"))
    }

    /// Upserts the identity and device for a successful login.
    #[allow(clippy::too_many_arguments)]
    pub async fn complete_login(
        &self,
        provider_id: &str,
        issuer: &str,
        subject: &str,
        email: Option<&str>,
        email_verified: bool,
        display_name: Option<&str>,
        avatar_url: Option<&str>,
        claims: serde_json::Value,
        new_device: NewDevice,
    ) -> Result<(UserRow, DeviceRow)> {
        let mut tx = self.pool.begin().await?;

        let existing = sqlx::query_as::<_, IdentityRow>(
            "SELECT id, user_id, provider_id, issuer, subject, email, email_verified
             FROM identities WHERE issuer = $1 AND subject = $2 FOR UPDATE",
        )
        .bind(issuer)
        .bind(subject)
        .fetch_optional(&mut *tx)
        .await?;

        let user = match existing {
            Some(identity) => {
                sqlx::query(
                    "UPDATE identities SET email = $2, email_verified = $3, claims = $4,
                     updated_at = now() WHERE id = $1",
                )
                .bind(identity.id)
                .bind(email)
                .bind(email_verified)
                .bind(&claims)
                .execute(&mut *tx)
                .await?;
                sqlx::query(
                    "UPDATE users SET email = COALESCE($2, email),
                     display_name = COALESCE($3, display_name),
                     avatar_url = COALESCE($4, avatar_url), updated_at = now()
                     WHERE id = $1",
                )
                .bind(identity.user_id)
                .bind(email)
                .bind(display_name)
                .bind(avatar_url)
                .execute(&mut *tx)
                .await?;
                self.user_in_tx(&mut tx, identity.user_id).await?
            }
            None => {
                let user_id = Uuid::now_v7();
                sqlx::query(
                    "INSERT INTO users (id, email, display_name, avatar_url)
                     VALUES ($1, $2, $3, $4)",
                )
                .bind(user_id)
                .bind(email)
                .bind(display_name)
                .bind(avatar_url)
                .execute(&mut *tx)
                .await?;
                sqlx::query(
                    "INSERT INTO identities
                     (id, user_id, provider_id, issuer, subject, email, email_verified, claims)
                     VALUES ($1, $2, $3, $4, $5, $6, $7, $8)",
                )
                .bind(Uuid::now_v7())
                .bind(user_id)
                .bind(provider_id)
                .bind(issuer)
                .bind(subject)
                .bind(email)
                .bind(email_verified)
                .bind(&claims)
                .execute(&mut *tx)
                .await?;
                self.user_in_tx(&mut tx, user_id).await?
            }
        };
        let _ = provider_id;

        let device = upsert_device_in(&mut tx, user.id, &new_device).await?;
        if new_device.kind == ConnectDeviceKind::Daemon {
            attach_daemon_machine(&mut tx, user.id, &device, &new_device).await?;
        }
        let device = self.device_in_tx(&mut tx, device.id).await?;
        tx.commit().await?;
        Ok((user, device))
    }

    async fn user_in_tx(&self, tx: &mut Transaction<'_, Postgres>, id: Uuid) -> Result<UserRow> {
        sqlx::query_as::<_, UserRow>(
            "SELECT id, email, display_name, avatar_url, created_at FROM users WHERE id = $1",
        )
        .bind(id)
        .fetch_one(&mut **tx)
        .await
        .map_err(Into::into)
    }

    async fn device_in_tx(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        id: Uuid,
    ) -> Result<DeviceRow> {
        sqlx::query_as::<_, DeviceRow>(
            "SELECT id, user_id, machine_id, kind, name, proof_public_key,
             noise_static_public_key, platform, last_seen, revoked_at, created_at
             FROM devices WHERE id = $1",
        )
        .bind(id)
        .fetch_one(&mut **tx)
        .await
        .map_err(Into::into)
    }

    /// Looks up a device.
    pub async fn device(&self, id: Uuid) -> Result<DeviceRow> {
        sqlx::query_as::<_, DeviceRow>(
            "SELECT id, user_id, machine_id, kind, name, proof_public_key,
             noise_static_public_key, platform, last_seen, revoked_at, created_at
             FROM devices WHERE id = $1",
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await?
        .ok_or_else(|| AppError::not_found("device_not_found", "device does not exist"))
    }

    /// Lists a user's devices.
    pub async fn devices(&self, user_id: Uuid) -> Result<Vec<DeviceRow>> {
        Ok(sqlx::query_as::<_, DeviceRow>(
            "SELECT id, user_id, machine_id, kind, name, proof_public_key,
             noise_static_public_key, platform, last_seen, revoked_at, created_at
             FROM devices WHERE user_id = $1 ORDER BY created_at ASC",
        )
        .bind(user_id)
        .fetch_all(&self.pool)
        .await?)
    }

    /// Renames a device the user owns.
    pub async fn rename_device(
        &self,
        user_id: Uuid,
        device_id: Uuid,
        name: &str,
    ) -> Result<DeviceRow> {
        let name = name.trim();
        if name.is_empty() || name.len() > 120 {
            return Err(AppError::bad_request(
                "invalid_device_name",
                "device name must be 1-120 characters",
            ));
        }
        let row = sqlx::query_as::<_, DeviceRow>(
            "UPDATE devices SET name = $3 WHERE id = $1 AND user_id = $2 AND revoked_at IS NULL
             RETURNING id, user_id, machine_id, kind, name, proof_public_key,
             noise_static_public_key, platform, last_seen, revoked_at, created_at",
        )
        .bind(device_id)
        .bind(user_id)
        .bind(name)
        .fetch_optional(&self.pool)
        .await?
        .ok_or_else(|| AppError::not_found("device_not_found", "device does not exist"))?;
        Ok(row)
    }

    /// Revokes a device, its sessions and every pairing. Returns the machines
    /// whose daemons must be told.
    ///
    /// The device's push registrations are deleted in the same transaction: a
    /// lost phone that is revoked from the desktop must stop receiving thread
    /// titles at once, not at its next sign-out.
    pub async fn revoke_device(&self, user_id: Uuid, device_id: Uuid) -> Result<Vec<Uuid>> {
        let mut tx = self.pool.begin().await?;
        let device = sqlx::query_as::<_, DeviceRow>(
            "SELECT id, user_id, machine_id, kind, name, proof_public_key,
             noise_static_public_key, platform, last_seen, revoked_at, created_at
             FROM devices WHERE id = $1 AND user_id = $2 FOR UPDATE",
        )
        .bind(device_id)
        .bind(user_id)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or_else(|| AppError::not_found("device_not_found", "device does not exist"))?;
        if device.revoked_at.is_some() {
            tx.rollback().await.ok();
            return Err(AppError::conflict(
                "device_revoked",
                "device is already revoked",
            ));
        }
        let mut machine_ids: Vec<Uuid> =
            sqlx::query_scalar("SELECT machine_id FROM machine_devices WHERE device_id = $1")
                .bind(device_id)
                .fetch_all(&mut *tx)
                .await?;
        if let Some(machine_id) = device.machine_id {
            machine_ids.push(machine_id);
        }
        machine_ids.sort_unstable();
        machine_ids.dedup();

        sqlx::query("UPDATE devices SET revoked_at = now() WHERE id = $1")
            .bind(device_id)
            .execute(&mut *tx)
            .await?;
        sqlx::query("DELETE FROM machine_devices WHERE device_id = $1")
            .bind(device_id)
            .execute(&mut *tx)
            .await?;
        sqlx::query("DELETE FROM push_devices WHERE user_id = $1 AND device_id = $2")
            .bind(user_id)
            .bind(device_id.to_string())
            .execute(&mut *tx)
            .await?;
        sqlx::query(
            "UPDATE sessions SET revoked_at = now(), revoke_reason = 'device_revoked'
             WHERE device_id = $1 AND revoked_at IS NULL",
        )
        .bind(device_id)
        .execute(&mut *tx)
        .await?;
        sqlx::query(
            "INSERT INTO presence (device_id, user_id, machine_id, status, last_seen_at, updated_at)
             VALUES ($1, $2, $3, 'offline', now(), now())
             ON CONFLICT (device_id) DO UPDATE SET status = 'offline', last_seen_at = now(),
             updated_at = now()",
        )
        .bind(device_id)
        .bind(user_id)
        .bind(device.machine_id)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(machine_ids)
    }

    /// Registers or rotates one install's push token.
    ///
    /// A token names one install on the push provider's side, so a token seen
    /// on another row is that install's stale binding and is removed first.
    /// The upsert is keyed by `(user_id, device_id)`: registering again
    /// replaces the token instead of creating a second deliverable row.
    pub async fn register_push_device(
        &self,
        user_id: Uuid,
        device_id: &str,
        token: &str,
        platform: &str,
        package_name: Option<&str>,
        app_version: Option<&str>,
    ) -> Result<PushDeviceRow> {
        if device_id.trim().is_empty() || token.trim().is_empty() {
            return Err(AppError::bad_request(
                "push_device_invalid",
                "device_id and token must not be empty",
            ));
        }
        let mut tx = self.pool.begin().await?;
        sqlx::query(
            "DELETE FROM push_devices WHERE token = $1 AND NOT (user_id = $2 AND device_id = $3)",
        )
        .bind(token)
        .bind(user_id)
        .bind(device_id)
        .execute(&mut *tx)
        .await?;
        sqlx::query(
            "DELETE FROM push_devices WHERE user_id = $1 AND device_id = $2 AND token <> $3",
        )
        .bind(user_id)
        .bind(device_id)
        .bind(token)
        .execute(&mut *tx)
        .await?;
        let row = sqlx::query_as::<_, PushDeviceRow>(
            "INSERT INTO push_devices
             (id, user_id, device_id, token, platform, package_name, app_version)
             VALUES ($1,$2,$3,$4,$5,$6,$7)
             ON CONFLICT (user_id, device_id) DO UPDATE SET
                 token = $4, platform = $5, package_name = $6, app_version = $7, updated_at = now()
             RETURNING id, user_id, device_id, token, platform, package_name, app_version,
                       created_at, updated_at",
        )
        .bind(Uuid::now_v7())
        .bind(user_id)
        .bind(device_id)
        .bind(token)
        .bind(platform)
        .bind(package_name)
        .bind(app_version)
        .fetch_one(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(row)
    }

    /// Removes one install's registration; missing rows are not an error.
    pub async fn deregister_push_device(&self, user_id: Uuid, device_id: &str) -> Result<bool> {
        let removed = sqlx::query("DELETE FROM push_devices WHERE user_id = $1 AND device_id = $2")
            .bind(user_id)
            .bind(device_id)
            .execute(&self.pool)
            .await?
            .rows_affected();
        Ok(removed > 0)
    }

    /// Removes a token FCM reported as unregistered or unknown.
    pub async fn delete_push_device_by_token(&self, token: &str) -> Result<bool> {
        let removed = sqlx::query("DELETE FROM push_devices WHERE token = $1")
            .bind(token)
            .execute(&self.pool)
            .await?
            .rows_affected();
        Ok(removed > 0)
    }

    /// Every install registered by one account.
    ///
    /// A device whose `revoked_at` is set never receives an alert even if a
    /// registration row survived (revocation deletes it; this is the second
    /// line of defence).
    pub async fn push_devices(&self, user_id: Uuid) -> Result<Vec<PushDeviceRow>> {
        Ok(sqlx::query_as::<_, PushDeviceRow>(
            "SELECT p.id, p.user_id, p.device_id, p.token, p.platform, p.package_name,
                    p.app_version, p.created_at, p.updated_at
             FROM push_devices p
             JOIN devices d ON d.id::text = p.device_id
             WHERE p.user_id = $1 AND d.revoked_at IS NULL
             ORDER BY p.created_at",
        )
        .bind(user_id)
        .fetch_all(&self.pool)
        .await?)
    }

    /// One install's registration, when it exists.
    pub async fn push_device(
        &self,
        user_id: Uuid,
        device_id: &str,
    ) -> Result<Option<PushDeviceRow>> {
        Ok(sqlx::query_as::<_, PushDeviceRow>(
            "SELECT id, user_id, device_id, token, platform, package_name, app_version,
                    created_at, updated_at
             FROM push_devices WHERE user_id = $1 AND device_id = $2",
        )
        .bind(user_id)
        .bind(device_id)
        .fetch_optional(&self.pool)
        .await?)
    }

    /// Claims one activity transition for a thread.
    ///
    /// Returns true only when this transition has not been delivered before.
    /// The insert/update is one statement, so two relays or two retries of the
    /// same frame cannot both claim it. A later transition carries a different
    /// id even when its state repeats, so a thread alerts again in the same
    /// state.
    pub async fn claim_push_thread_state(
        &self,
        user_id: Uuid,
        thread_id: &str,
        state: &str,
        transition: &str,
    ) -> Result<bool> {
        let claimed: Option<bool> = sqlx::query_scalar(
            "INSERT INTO push_thread_states (user_id, thread_id, state, transition, updated_at)
             VALUES ($1,$2,$3,$4,now())
             ON CONFLICT (user_id, thread_id) DO UPDATE
                 SET state = excluded.state, transition = excluded.transition, updated_at = now()
                 WHERE push_thread_states.transition <> excluded.transition
             RETURNING true",
        )
        .bind(user_id)
        .bind(thread_id)
        .bind(state)
        .bind(transition)
        .fetch_optional(&self.pool)
        .await?;
        Ok(claimed.unwrap_or(false))
    }

    /// Marks a device seen.
    pub async fn touch_device(&self, device_id: Uuid) -> Result<()> {
        sqlx::query("UPDATE devices SET last_seen = now() WHERE id = $1")
            .bind(device_id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Records or replaces a device's Noise static public key.
    pub async fn set_device_noise_key(&self, device_id: Uuid, key: &[u8]) -> Result<()> {
        sqlx::query("UPDATE devices SET noise_static_public_key = $2 WHERE id = $1")
            .bind(device_id)
            .bind(key)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// The scopes a device holds, as typed values.
    ///
    /// A device paired with machines holds the union of the scopes on its
    /// links. An unpaired device holds `view` alone when it is a phone (it can
    /// only claim a pairing) and everything otherwise: the account owner's
    /// desktop, browser and daemon are not scope-limited until they are paired
    /// like a remote.
    pub async fn device_scopes(&self, device_id: Uuid) -> Result<Vec<PairScope>> {
        let rows: Vec<Vec<String>> =
            sqlx::query_scalar("SELECT scopes FROM machine_devices WHERE device_id = $1")
                .bind(device_id)
                .fetch_all(&self.pool)
                .await?;
        if !rows.is_empty() {
            return Ok(PairScope::normalize(
                rows.iter().flat_map(|row| parse_scopes(row)),
            ));
        }
        let kind: Option<String> = sqlx::query_scalar("SELECT kind FROM devices WHERE id = $1")
            .bind(device_id)
            .fetch_optional(&self.pool)
            .await?;
        Ok(match kind.as_deref() {
            Some("mobile") => vec![PairScope::View],
            _ => PairScope::ALL.to_vec(),
        })
    }

    /// [`Self::device_scopes`] spelled for a token claim.
    pub async fn token_scopes(&self, device_id: Uuid) -> Result<Vec<String>> {
        Ok(scope_strings(&self.device_scopes(device_id).await?))
    }

    /// What a device may do on one machine.
    ///
    /// Machine-scoped actions are authorised against this, never against the
    /// union [`Self::device_scopes`] returns: admin on machine A must not let a
    /// device mint an admin pairing for machine B.
    ///
    /// - A linked device holds exactly its link's scopes.
    /// - A machine's own daemon holds every scope on its machine.
    /// - A device linked only to *other* machines holds nothing here.
    /// - A device linked to no machine at all keeps the unpaired default: the
    ///   account owner's desktop, browser and daemon are not scope-limited until
    ///   they are paired like a remote, and an unpaired phone holds `view`.
    pub async fn machine_scopes(
        &self,
        machine_id: Uuid,
        device_id: Uuid,
    ) -> Result<Vec<PairScope>> {
        let link: Option<Vec<String>> = sqlx::query_scalar(
            "SELECT scopes FROM machine_devices WHERE machine_id = $1 AND device_id = $2",
        )
        .bind(machine_id)
        .bind(device_id)
        .fetch_optional(&self.pool)
        .await?;
        if let Some(link) = link {
            return Ok(PairScope::normalize(parse_scopes(&link)));
        }
        let own: Option<Option<Uuid>> =
            sqlx::query_scalar("SELECT machine_id FROM devices WHERE id = $1")
                .bind(device_id)
                .fetch_optional(&self.pool)
                .await?;
        if own.flatten() == Some(machine_id) {
            return Ok(PairScope::ALL.to_vec());
        }
        let linked_elsewhere: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM machine_devices WHERE device_id = $1")
                .bind(device_id)
                .fetch_one(&self.pool)
                .await?;
        if linked_elsewhere > 0 {
            return Ok(Vec::new());
        }
        let kind: Option<String> = sqlx::query_scalar("SELECT kind FROM devices WHERE id = $1")
            .bind(device_id)
            .fetch_optional(&self.pool)
            .await?;
        Ok(match kind.as_deref() {
            Some("mobile") => vec![PairScope::View],
            _ => PairScope::ALL.to_vec(),
        })
    }

    /// Every machine a device is linked to, or runs.
    pub async fn device_machines(&self, device_id: Uuid) -> Result<Vec<Uuid>> {
        let mut machines: Vec<Uuid> =
            sqlx::query_scalar("SELECT machine_id FROM machine_devices WHERE device_id = $1")
                .bind(device_id)
                .fetch_all(&self.pool)
                .await?;
        let own: Option<Option<Uuid>> =
            sqlx::query_scalar("SELECT machine_id FROM devices WHERE id = $1")
                .bind(device_id)
                .fetch_optional(&self.pool)
                .await?;
        machines.extend(own.flatten());
        machines.sort_unstable();
        machines.dedup();
        Ok(machines)
    }

    /// The machine a pairing belongs to, for authorising its revocation.
    pub async fn pairing_machine(&self, user_id: Uuid, pairing_id: Uuid) -> Result<Uuid> {
        sqlx::query_scalar(
            "SELECT machine_id FROM pairings
             WHERE id = $1 AND machine_id IN (SELECT id FROM machines WHERE user_id = $2)",
        )
        .bind(pairing_id)
        .bind(user_id)
        .fetch_optional(&self.pool)
        .await?
        .ok_or_else(|| {
            AppError::not_found(
                "pairing_not_found",
                "pairing does not exist or is not pending",
            )
        })
    }

    /// The scopes one machine link carries.
    pub async fn link_scopes(&self, machine_id: Uuid, device_id: Uuid) -> Result<Vec<PairScope>> {
        let row: Option<Vec<String>> = sqlx::query_scalar(
            "SELECT scopes FROM machine_devices WHERE machine_id = $1 AND device_id = $2",
        )
        .bind(machine_id)
        .bind(device_id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(|row| parse_scopes(&row)).unwrap_or_default())
    }

    /// The paired devices of a machine as wire records carrying link scopes.
    pub async fn paired_device_wires(&self, machine_id: Uuid) -> Result<Vec<ConnectDevice>> {
        let devices = self.paired_devices(machine_id).await?;
        let mut out = Vec::with_capacity(devices.len());
        for device in devices {
            let mut wire = device.to_wire();
            wire.scopes = self.link_scopes(machine_id, device.id).await?;
            out.push(wire);
        }
        Ok(out)
    }

    /// Lists the live client devices paired with a machine.
    pub async fn paired_devices(&self, machine_id: Uuid) -> Result<Vec<DeviceRow>> {
        Ok(sqlx::query_as::<_, DeviceRow>(
            "SELECT d.id, d.user_id, d.machine_id, d.kind, d.name, d.proof_public_key,
             d.noise_static_public_key, d.platform, d.last_seen, d.revoked_at, d.created_at
             FROM devices d
             JOIN machine_devices md ON md.device_id = d.id
             WHERE md.machine_id = $1 AND d.revoked_at IS NULL
             ORDER BY d.created_at ASC",
        )
        .bind(machine_id)
        .fetch_all(&self.pool)
        .await?)
    }

    /// Checks that a client device may reach a machine.
    pub async fn device_may_reach(
        &self,
        user_id: Uuid,
        device_id: Uuid,
        machine_id: Uuid,
    ) -> Result<bool> {
        let count: Option<i64> = sqlx::query_scalar(
            "SELECT COUNT(*) FROM machine_devices md
             JOIN machines m ON m.id = md.machine_id
             JOIN devices d ON d.id = md.device_id
             WHERE md.machine_id = $1 AND md.device_id = $2 AND m.user_id = $3
             AND d.user_id = $3 AND d.revoked_at IS NULL",
        )
        .bind(machine_id)
        .bind(device_id)
        .bind(user_id)
        .fetch_one(&self.pool)
        .await?;
        if count.unwrap_or(0) > 0 {
            return Ok(true);
        }
        // A daemon owns its own machine and may also reach it.
        let own: Option<i64> = sqlx::query_scalar(
            "SELECT COUNT(*) FROM machines m JOIN devices d ON d.id = m.daemon_device_id
             WHERE m.id = $1 AND m.user_id = $2 AND d.id = $3 AND d.revoked_at IS NULL",
        )
        .bind(machine_id)
        .bind(user_id)
        .bind(device_id)
        .fetch_one(&self.pool)
        .await?;
        Ok(own.unwrap_or(0) > 0)
    }

    /// Finds a machine by id.
    pub async fn machine(&self, id: Uuid) -> Result<MachineRow> {
        sqlx::query_as::<_, MachineRow>(
            "SELECT id, user_id, name, daemon_device_id, created_at, updated_at
             FROM machines WHERE id = $1",
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await?
        .ok_or_else(|| AppError::not_found("machine_not_found", "machine does not exist"))
    }

    /// Lists a user's machines with hosts. `online` is a snapshot of the relay
    /// hub's connected machines, taken before this query.
    pub async fn machines(
        &self,
        user_id: Uuid,
        online: &std::collections::HashSet<Uuid>,
    ) -> Result<Vec<MachineSummary>> {
        let rows = sqlx::query_as::<_, MachineRow>(
            "SELECT id, user_id, name, daemon_device_id, created_at, updated_at
             FROM machines WHERE user_id = $1 ORDER BY created_at ASC",
        )
        .bind(user_id)
        .fetch_all(&self.pool)
        .await?;
        let mut out = Vec::with_capacity(rows.len());
        for row in rows {
            let hosts = self.hosts(row.id).await?;
            let addresses = self.addresses(row.id).await?;
            let last_seen = match row.daemon_device_id {
                Some(device_id) => self
                    .device(device_id)
                    .await
                    .ok()
                    .and_then(|device| device.last_seen),
                None => None,
            };
            out.push(MachineSummary {
                id: row.id.to_string(),
                name: row.name.clone(),
                online: online.contains(&row.id),
                last_seen,
                daemon_device_id: row.daemon_device_id.map(|id| id.to_string()),
                hosts: hosts.iter().map(HostRow::to_wire).collect(),
                addresses,
            });
        }
        Ok(out)
    }

    /// Advertised connection candidates for a machine, preferred first.
    pub async fn addresses(&self, machine_id: Uuid) -> Result<Vec<String>> {
        Ok(sqlx::query_scalar::<_, String>(
            "SELECT address FROM machine_addresses
             WHERE machine_id = $1 ORDER BY ordinal ASC, address ASC",
        )
        .bind(machine_id)
        .fetch_all(&self.pool)
        .await?)
    }

    /// Replaces the address list for a machine, preserving order.
    pub async fn replace_addresses(
        &self,
        machine_id: Uuid,
        addresses: &[String],
    ) -> Result<Vec<String>> {
        let mut tx = self.pool.begin().await?;
        sqlx::query("DELETE FROM machine_addresses WHERE machine_id = $1")
            .bind(machine_id)
            .execute(&mut *tx)
            .await?;
        for (ordinal, address) in addresses.iter().enumerate() {
            let address = address.trim();
            if address.is_empty() || address.len() > 512 {
                continue;
            }
            sqlx::query(
                "INSERT INTO machine_addresses (id, machine_id, address, ordinal)
                 VALUES ($1, $2, $3, $4)
                 ON CONFLICT (machine_id, address) DO UPDATE
                 SET ordinal = EXCLUDED.ordinal, updated_at = now()",
            )
            .bind(Uuid::now_v7())
            .bind(machine_id)
            .bind(address)
            .bind(ordinal as i32)
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        self.addresses(machine_id).await
    }

    /// Hosts of a machine.
    pub async fn hosts(&self, machine_id: Uuid) -> Result<Vec<HostRow>> {
        Ok(sqlx::query_as::<_, HostRow>(
            "SELECT id, machine_id, name, kind, distro FROM machine_hosts
             WHERE machine_id = $1 ORDER BY created_at ASC",
        )
        .bind(machine_id)
        .fetch_all(&self.pool)
        .await?)
    }

    /// Replaces the host list for a machine (daemons report WSL distros here).
    pub async fn replace_hosts(
        &self,
        machine_id: Uuid,
        hosts: &[(String, MachineHostKind, Option<String>)],
    ) -> Result<Vec<HostRow>> {
        let mut tx = self.pool.begin().await?;
        sqlx::query("DELETE FROM machine_hosts WHERE machine_id = $1")
            .bind(machine_id)
            .execute(&mut *tx)
            .await?;
        for (name, kind, distro) in hosts {
            let kind = match kind {
                MachineHostKind::Host => "host",
                MachineHostKind::Wsl => "wsl",
            };
            sqlx::query(
                "INSERT INTO machine_hosts (id, machine_id, name, kind, distro)
                 VALUES ($1, $2, $3, $4, $5)",
            )
            .bind(Uuid::now_v7())
            .bind(machine_id)
            .bind(name)
            .bind(kind)
            .bind(distro)
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        self.hosts(machine_id).await
    }

    /// Creates a pairing with a fresh single-use short code.
    ///
    /// Returns the row and the plaintext code; only its hash is stored. Forty
    /// random bits among a handful of live pairings makes a collision a
    /// unique-index violation, so a new code is drawn.
    pub async fn create_pairing(
        &self,
        machine_id: Uuid,
        created_by_device_id: Uuid,
        secret_hash: &str,
        scopes: &[PairScope],
        ttl: std::time::Duration,
    ) -> Result<(PairingRow, String)> {
        let expires_at =
            Utc::now() + Duration::from_std(ttl).unwrap_or_else(|_| Duration::seconds(300));
        let scopes = scope_strings(&PairScope::normalize(scopes.iter().copied()));
        for _ in 0..5 {
            let code = generate_short_code();
            let inserted = sqlx::query_as::<_, PairingRow>(
                "INSERT INTO pairings (id, machine_id, created_by_device_id, secret_hash,
                 code_hash, scopes, expires_at)
                 VALUES ($1, $2, $3, $4, $5, $6, $7)
                 RETURNING id, machine_id, created_by_device_id, secret_hash, state, expires_at,
                 claimed_by_device_id, claimed_at, attempts, created_at, scopes",
            )
            .bind(Uuid::now_v7())
            .bind(machine_id)
            .bind(created_by_device_id)
            .bind(secret_hash)
            .bind(hash_pairing_code(&code))
            .bind(&scopes)
            .bind(expires_at)
            .fetch_one(&self.pool)
            .await;
            match inserted {
                Ok(row) => return Ok((row, code)),
                Err(sqlx::Error::Database(error)) if error.is_unique_violation() => {}
                Err(error) => return Err(error.into()),
            }
        }
        Err(AppError::internal(
            "could not allocate a unique pairing code",
        ))
    }

    /// Claims a pairing by machine and secret.
    ///
    /// Returns `(pairing, machine)`. Failed attempts are counted and burn every
    /// pending pairing for the machine once the limit is reached.
    pub async fn claim_pairing(
        &self,
        machine_id: Uuid,
        user_id: Uuid,
        device_id: Uuid,
        secret: &str,
        max_attempts: i32,
    ) -> Result<(PairingRow, MachineRow)> {
        let mut tx = self.pool.begin().await?;
        let candidates = sqlx::query_as::<_, PairingRow>(
            "SELECT id, machine_id, created_by_device_id, secret_hash, state, expires_at,
             claimed_by_device_id, claimed_at, attempts, created_at, scopes
             FROM pairings WHERE machine_id = $1 AND state = 'pending' AND expires_at > now()
             ORDER BY created_at DESC FOR UPDATE",
        )
        .bind(machine_id)
        .fetch_all(&mut *tx)
        .await?;

        let matched = candidates
            .iter()
            .find(|pairing| verify_pairing_secret(secret, &pairing.secret_hash));
        let Some(matched) = matched else {
            if candidates.is_empty() {
                tx.rollback().await.ok();
                return Err(AppError::bad_request_hint(
                    "pairing_invalid",
                    "pairing code is invalid or expired",
                    "Generate a new pairing on the daemon.",
                ));
            }
            sqlx::query(
                "UPDATE pairings SET attempts = attempts + 1,
                 state = CASE WHEN attempts + 1 >= $2 THEN 'revoked' ELSE state END
                 WHERE machine_id = $1 AND state = 'pending' AND expires_at > now()",
            )
            .bind(machine_id)
            .bind(max_attempts.max(1))
            .execute(&mut *tx)
            .await?;
            tx.commit().await?;
            return Err(AppError::bad_request_hint(
                "pairing_invalid",
                "pairing code is invalid or expired",
                "Generate a new pairing on the daemon.",
            ));
        };
        if matched.attempts >= max_attempts.max(1) {
            tx.rollback().await.ok();
            return Err(AppError::bad_request_hint(
                "pairing_burned",
                "too many failed pairing attempts",
                "Generate a new pairing on the daemon.",
            ));
        }

        let out = Self::finish_claim(&mut tx, matched, user_id, device_id).await?;
        tx.commit().await?;
        Ok(out)
    }

    /// Claims a pairing by its typed short code.
    ///
    /// The code identifies the pairing, so there is no machine to burn on a
    /// wrong guess; the caller's per-user and per-IP rate limits are the
    /// brake. Another user's pairing answers exactly like an unknown code.
    pub async fn claim_pairing_by_code(
        &self,
        user_id: Uuid,
        device_id: Uuid,
        code_hash: &str,
    ) -> Result<(PairingRow, MachineRow)> {
        let invalid = || {
            AppError::bad_request_hint(
                "pairing_invalid",
                "pairing code is invalid or expired",
                "Check the code on the daemon, or generate a new one.",
            )
        };
        let mut tx = self.pool.begin().await?;
        let matched = sqlx::query_as::<_, PairingRow>(
            "SELECT p.id, p.machine_id, p.created_by_device_id, p.secret_hash, p.state,
             p.expires_at, p.claimed_by_device_id, p.claimed_at, p.attempts, p.created_at, p.scopes
             FROM pairings p JOIN machines m ON m.id = p.machine_id
             WHERE p.code_hash = $1 AND p.state = 'pending' AND p.expires_at > now()
             AND m.user_id = $2 FOR UPDATE OF p",
        )
        .bind(code_hash)
        .bind(user_id)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or_else(invalid)?;
        let out = Self::finish_claim(&mut tx, &matched, user_id, device_id).await?;
        tx.commit().await?;
        Ok(out)
    }

    /// Marks `matched` claimed and links the device with the pairing's scopes.
    async fn finish_claim(
        tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
        matched: &PairingRow,
        user_id: Uuid,
        device_id: Uuid,
    ) -> Result<(PairingRow, MachineRow)> {
        let machine_id = matched.machine_id;
        // The claimant must own the machine.
        let machine = sqlx::query_as::<_, MachineRow>(
            "SELECT id, user_id, name, daemon_device_id, created_at, updated_at
             FROM machines WHERE id = $1 AND user_id = $2 FOR UPDATE",
        )
        .bind(machine_id)
        .bind(user_id)
        .fetch_optional(&mut **tx)
        .await?
        .ok_or_else(|| {
            AppError::forbidden("machine_forbidden", "machine belongs to another user")
        })?;

        let claimed = sqlx::query_as::<_, PairingRow>(
            "UPDATE pairings SET state = 'claimed', claimed_by_device_id = $2, claimed_at = now()
             WHERE id = $1 AND state = 'pending' AND expires_at > now()
             RETURNING id, machine_id, created_by_device_id, secret_hash, state, expires_at,
             claimed_by_device_id, claimed_at, attempts, created_at, scopes",
        )
        .bind(matched.id)
        .bind(device_id)
        .fetch_optional(&mut **tx)
        .await?
        .ok_or_else(|| AppError::conflict("pairing_used", "pairing was already claimed"))?;

        // Re-pairing replaces the scopes: the newest pairing is what the owner
        // chose last.
        sqlx::query(
            "INSERT INTO machine_devices (machine_id, device_id, scopes) VALUES ($1, $2, $3)
             ON CONFLICT (machine_id, device_id) DO UPDATE SET scopes = EXCLUDED.scopes,
             paired_at = now()",
        )
        .bind(machine_id)
        .bind(device_id)
        .bind(&claimed.scopes)
        .execute(&mut **tx)
        .await?;
        Ok((claimed, machine))
    }

    /// Lists a user's pairings.
    pub async fn pairings(&self, user_id: Uuid) -> Result<Vec<PairingRow>> {
        Ok(sqlx::query_as::<_, PairingRow>(
            "SELECT p.id, p.machine_id, p.created_by_device_id, p.secret_hash, p.state,
             p.expires_at, p.claimed_by_device_id, p.claimed_at, p.attempts, p.created_at, p.scopes
             FROM pairings p JOIN machines m ON m.id = p.machine_id
             WHERE m.user_id = $1 ORDER BY p.created_at DESC LIMIT 100",
        )
        .bind(user_id)
        .fetch_all(&self.pool)
        .await?)
    }

    /// Creates a login flow.
    #[allow(clippy::too_many_arguments)]
    pub async fn create_auth_flow(
        &self,
        provider_id: &str,
        state: &str,
        nonce: &str,
        pkce_verifier: &str,
        redirect_uri: &str,
        new_device: &NewDevice,
        ttl: std::time::Duration,
    ) -> Result<AuthFlowRow> {
        let expires_at =
            Utc::now() + Duration::from_std(ttl).unwrap_or_else(|_| Duration::seconds(600));
        Ok(sqlx::query_as::<_, AuthFlowRow>(
            "INSERT INTO auth_flows
             (id, provider_id, state, nonce, pkce_verifier, redirect_uri, device_kind,
              device_name, proof_public_key, noise_static_public_key, platform, machine_name,
              expires_at)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13)
             RETURNING id, provider_id, state, nonce, pkce_verifier, redirect_uri, device_kind,
             device_name, proof_public_key, noise_static_public_key, platform, machine_name,
             expires_at",
        )
        .bind(Uuid::now_v7())
        .bind(provider_id)
        .bind(state)
        .bind(nonce)
        .bind(pkce_verifier)
        .bind(redirect_uri)
        .bind(new_device.kind.as_str())
        .bind(&new_device.name)
        .bind(&new_device.proof_public_key)
        .bind(&new_device.noise_static_public_key)
        .bind(&new_device.platform)
        .bind(&new_device.machine_name)
        .bind(expires_at)
        .fetch_one(&self.pool)
        .await?)
    }

    /// Consumes a login flow atomically.
    pub async fn consume_auth_flow(&self, provider_id: &str, state: &str) -> Result<AuthFlowRow> {
        sqlx::query_as::<_, AuthFlowRow>(
            "UPDATE auth_flows SET consumed_at = now()
             WHERE state = $1 AND provider_id = $2 AND consumed_at IS NULL AND expires_at > now()
             RETURNING id, provider_id, state, nonce, pkce_verifier, redirect_uri, device_kind,
             device_name, proof_public_key, noise_static_public_key, platform, machine_name,
             expires_at",
        )
        .bind(state)
        .bind(provider_id)
        .fetch_optional(&self.pool)
        .await?
        .ok_or_else(|| {
            AppError::bad_request_hint(
                "auth_flow_invalid",
                "login flow is invalid or expired",
                "Start the login again.",
            )
        })
    }

    /// Stores a device grant.
    pub async fn create_device_grant(&self, input: NewDeviceGrant<'_>) -> Result<DeviceGrantRow> {
        Ok(sqlx::query_as::<_, DeviceGrantRow>(
            "INSERT INTO device_authorizations
             (id, provider_id, device_code, user_code, verification_uri,
              verification_uri_complete, interval_secs, expires_at, device_kind, device_name,
              proof_public_key, noise_static_public_key, platform, machine_name)
             VALUES ($1,$2,$3,$4,$5,$6,$7, now() + make_interval(secs => $8), $9,$10,$11,$12,$13,$14)
             RETURNING id, provider_id, device_code, user_code, verification_uri,
             verification_uri_complete, interval_secs, expires_at, status, device_kind,
             device_name, proof_public_key, noise_static_public_key, platform, machine_name,
             user_id, device_id, session_id, refresh_ciphertext, last_polled_at",
        )
        .bind(Uuid::now_v7())
        .bind(input.provider_id)
        .bind(input.device_code)
        .bind(input.user_code)
        .bind(input.verification_uri)
        .bind(input.verification_uri_complete)
        .bind(input.interval_secs.max(1))
        .bind(input.expires_in.max(1) as f64)
        .bind(input.device.kind.as_str())
        .bind(&input.device.name)
        .bind(&input.device.proof_public_key)
        .bind(&input.device.noise_static_public_key)
        .bind(&input.device.platform)
        .bind(&input.device.machine_name)
        .fetch_one(&self.pool)
        .await?)
    }

    /// Loads a device grant.
    pub async fn device_grant(&self, grant_id: Uuid) -> Result<DeviceGrantRow> {
        sqlx::query_as::<_, DeviceGrantRow>(
            "SELECT id, provider_id, device_code, user_code, verification_uri,
             verification_uri_complete, interval_secs, expires_at, status, device_kind,
             device_name, proof_public_key, noise_static_public_key, platform, machine_name,
             user_id, device_id, session_id, refresh_ciphertext, last_polled_at
             FROM device_authorizations WHERE id = $1",
        )
        .bind(grant_id)
        .fetch_optional(&self.pool)
        .await?
        .ok_or_else(|| AppError::not_found("grant_not_found", "device grant does not exist"))
    }

    /// Records a poll time and returns the grant.
    pub async fn note_device_poll(&self, grant_id: Uuid) -> Result<DeviceGrantRow> {
        sqlx::query_as::<_, DeviceGrantRow>(
            "UPDATE device_authorizations SET last_polled_at = now(), updated_at = now()
             WHERE id = $1
             RETURNING id, provider_id, device_code, user_code, verification_uri,
             verification_uri_complete, interval_secs, expires_at, status, device_kind,
             device_name, proof_public_key, noise_static_public_key, platform, machine_name,
             user_id, device_id, session_id, refresh_ciphertext, last_polled_at",
        )
        .bind(grant_id)
        .fetch_one(&self.pool)
        .await
        .map_err(Into::into)
    }

    /// Marks a device grant approved/denied/expired.
    pub async fn set_device_grant_status(
        &self,
        grant_id: Uuid,
        status: &str,
        user_id: Option<Uuid>,
        device_id: Option<Uuid>,
    ) -> Result<()> {
        sqlx::query(
            "UPDATE device_authorizations SET status = $2, user_id = COALESCE($3, user_id),
             device_id = COALESCE($4, device_id), updated_at = now() WHERE id = $1",
        )
        .bind(grant_id)
        .bind(status)
        .bind(user_id)
        .bind(device_id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Marks a device grant approved and records the issued session, so a
    /// repeated poll can reissue an access token instead of a second session.
    pub async fn approve_device_grant(
        &self,
        grant_id: Uuid,
        user_id: Uuid,
        device_id: Uuid,
        session_id: Uuid,
        refresh_ciphertext: &[u8],
    ) -> Result<()> {
        sqlx::query(
            "UPDATE device_authorizations SET status = 'approved', user_id = $2,
             device_id = $3, session_id = $4, refresh_ciphertext = $5, updated_at = now()
             WHERE id = $1",
        )
        .bind(grant_id)
        .bind(user_id)
        .bind(device_id)
        .bind(session_id)
        .bind(refresh_ciphertext)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Loads a session row by id.
    pub async fn session(&self, id: Uuid) -> Result<SessionRow> {
        sqlx::query_as::<_, SessionRow>(
            "SELECT id, family_id, user_id, device_id, refresh_hash, access_jti, issued_at,
             expires_at, rotated_at, replaced_by, successor_secret, revoked_at, revoke_reason
             FROM sessions WHERE id = $1",
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await?
        .ok_or_else(|| AppError::not_found("session_not_found", "session does not exist"))
    }

    /// Revokes a pending pairing the caller owns.
    pub async fn revoke_pairing(&self, user_id: Uuid, pairing_id: Uuid) -> Result<PairingRow> {
        sqlx::query_as::<_, PairingRow>(
            "UPDATE pairings SET state = 'revoked'
             WHERE id = $1 AND state = 'pending'
             AND machine_id IN (SELECT id FROM machines WHERE user_id = $2)
             RETURNING id, machine_id, created_by_device_id, secret_hash, state, expires_at,
             claimed_by_device_id, claimed_at, attempts, created_at, scopes",
        )
        .bind(pairing_id)
        .bind(user_id)
        .fetch_optional(&self.pool)
        .await?
        .ok_or_else(|| {
            AppError::not_found(
                "pairing_not_found",
                "pairing does not exist or is not pending",
            )
        })
    }

    /// Issues a brand-new session family.
    pub async fn issue_session(
        &self,
        keystore: &Keystore,
        config: &Config,
        user_id: Uuid,
        device_id: Uuid,
        user_agent: Option<&str>,
        ip: Option<&str>,
    ) -> Result<IssuedSession> {
        let session_id = Uuid::now_v7();
        let family_id = Uuid::now_v7();
        let access_jti = Uuid::now_v7();
        let refresh = random_b64(32);
        let expires_at = Utc::now() + config.refresh_ttl();
        let row = sqlx::query_as::<_, SessionRow>(
            "INSERT INTO sessions (id, family_id, user_id, device_id, refresh_hash, access_jti,
             expires_at, user_agent, ip)
             VALUES ($1,$2,$3,$4,$5,$6, $7, $8, $9)
             RETURNING id, family_id, user_id, device_id, refresh_hash, access_jti, issued_at,
             expires_at, rotated_at, replaced_by, successor_secret, revoked_at, revoke_reason",
        )
        .bind(session_id)
        .bind(family_id)
        .bind(user_id)
        .bind(device_id)
        .bind(hash_refresh_token(&refresh))
        .bind(access_jti)
        .bind(expires_at)
        .bind(user_agent)
        .bind(ip)
        .fetch_one(&self.pool)
        .await?;
        let access_token = issue_access_token(
            keystore,
            &user_id.to_string(),
            &device_id.to_string(),
            &session_id.to_string(),
            &family_id.to_string(),
            &access_jti.to_string(),
            &self.token_scopes(device_id).await?,
            config.session_ttl_secs,
        )?;
        Ok(IssuedSession {
            tokens: ConnectSessionTokens {
                access_token,
                refresh_token: refresh,
                expires_in: config.session_ttl_secs,
                token_type: "Bearer".to_string(),
            },
            session: row,
        })
    }

    /// Rotates a refresh token with process-wide single-flight semantics.
    ///
    /// A transaction-scoped advisory lock serialises rotations in one family,
    /// and a short idempotent window returns the same successor for a retry
    /// (ADR-011 quirk 11). Outside the window, reuse revokes the family.
    pub async fn rotate_session(
        &self,
        keystore: &Keystore,
        config: &Config,
        envelope: &Envelope,
        refresh_token: &str,
        user_agent: Option<&str>,
        ip: Option<&str>,
    ) -> Result<IssuedSession> {
        let hash = hash_refresh_token(refresh_token);
        let family_id: Option<Uuid> =
            sqlx::query_scalar("SELECT family_id FROM sessions WHERE refresh_hash = $1")
                .bind(&hash)
                .fetch_optional(&self.pool)
                .await?;
        let Some(family_id) = family_id else {
            return Err(AppError::unauthorized(
                "invalid_grant",
                "refresh token is invalid",
            ));
        };

        let mut tx = self.pool.begin().await?;
        sqlx::query("SELECT pg_advisory_xact_lock(hashtext($1)::bigint)")
            .bind(family_id.to_string())
            .execute(&mut *tx)
            .await?;

        let current = sqlx::query_as::<_, SessionRow>(
            "SELECT id, family_id, user_id, device_id, refresh_hash, access_jti, issued_at,
             expires_at, rotated_at, replaced_by, successor_secret, revoked_at, revoke_reason
             FROM sessions WHERE refresh_hash = $1 FOR UPDATE",
        )
        .bind(&hash)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or_else(|| AppError::unauthorized("invalid_grant", "refresh token is invalid"))?;

        if current.revoked_at.is_some() || current.expires_at <= Utc::now() {
            tx.rollback().await.ok();
            return Err(AppError::unauthorized(
                "invalid_grant",
                "refresh token was revoked or expired",
            ));
        }

        match classify_reuse(
            current.rotated_at,
            Utc::now(),
            Duration::seconds(config.refresh_grace_secs),
        ) {
            ReuseDecision::ReplaySuccessor => {
                let successor_id = current.replaced_by.ok_or_else(|| {
                    AppError::unauthorized("invalid_grant", "no successor session")
                })?;
                let successor = sqlx::query_as::<_, SessionRow>(
                    "SELECT id, family_id, user_id, device_id, refresh_hash, access_jti, issued_at,
                     expires_at, rotated_at, replaced_by, successor_secret, revoked_at, revoke_reason
                     FROM sessions WHERE id = $1",
                )
                .bind(successor_id)
                .fetch_one(&mut *tx)
                .await?;
                if successor.revoked_at.is_some() || successor.rotated_at.is_some() {
                    tx.rollback().await.ok();
                    return Err(AppError::unauthorized(
                        "invalid_grant",
                        "refresh token was already rotated",
                    ));
                }
                let sealed: SealedSecret = current
                    .successor_secret
                    .as_deref()
                    .and_then(|bytes| serde_json::from_slice(bytes).ok())
                    .ok_or_else(|| {
                        AppError::unauthorized("invalid_grant", "successor token is unavailable")
                    })?;
                let aad = format!("successor:{}", current.id);
                let successor_refresh = envelope.open_str(&sealed, aad.as_bytes())?;
                let access_token = issue_access_token(
                    keystore,
                    &successor.user_id.to_string(),
                    &successor.device_id.to_string(),
                    &successor.id.to_string(),
                    &successor.family_id.to_string(),
                    &successor.access_jti.to_string(),
                    &self.token_scopes(successor.device_id).await?,
                    config.session_ttl_secs,
                )?;
                tx.commit().await?;
                return Ok(IssuedSession {
                    tokens: ConnectSessionTokens {
                        access_token,
                        refresh_token: successor_refresh,
                        expires_in: config.session_ttl_secs,
                        token_type: "Bearer".to_string(),
                    },
                    session: successor,
                });
            }
            ReuseDecision::RevokeFamily => {
                sqlx::query(
                    "UPDATE sessions SET revoked_at = now(), revoke_reason = 'refresh_reuse'
                     WHERE family_id = $1 AND revoked_at IS NULL",
                )
                .bind(current.family_id)
                .execute(&mut *tx)
                .await?;
                tx.commit().await?;
                return Err(AppError::unauthorized(
                    "refresh_reused",
                    "refresh token reuse revoked the session family",
                ));
            }
            ReuseDecision::Rotate => {}
        }

        let new_refresh = random_b64(32);
        let new_id = Uuid::now_v7();
        let new_jti = Uuid::now_v7();
        let expires_at = Utc::now() + config.refresh_ttl();
        let successor = sqlx::query_as::<_, SessionRow>(
            "INSERT INTO sessions (id, family_id, user_id, device_id, refresh_hash, access_jti,
             expires_at, user_agent, ip)
             VALUES ($1,$2,$3,$4,$5,$6, $7, $8, $9)
             RETURNING id, family_id, user_id, device_id, refresh_hash, access_jti, issued_at,
             expires_at, rotated_at, replaced_by, successor_secret, revoked_at, revoke_reason",
        )
        .bind(new_id)
        .bind(current.family_id)
        .bind(current.user_id)
        .bind(current.device_id)
        .bind(hash_refresh_token(&new_refresh))
        .bind(new_jti)
        .bind(expires_at)
        .bind(user_agent)
        .bind(ip)
        .fetch_one(&mut *tx)
        .await?;

        let aad = format!("successor:{}", current.id);
        let sealed = envelope.seal_str(&new_refresh, aad.as_bytes())?;
        let sealed =
            serde_json::to_vec(&sealed).map_err(|_| AppError::internal("seal successor"))?;
        sqlx::query(
            "UPDATE sessions SET rotated_at = now(), replaced_by = $2, successor_secret = $3
             WHERE id = $1",
        )
        .bind(current.id)
        .bind(new_id)
        .bind(sealed)
        .execute(&mut *tx)
        .await?;

        let access_token = issue_access_token(
            keystore,
            &successor.user_id.to_string(),
            &successor.device_id.to_string(),
            &successor.id.to_string(),
            &successor.family_id.to_string(),
            &successor.access_jti.to_string(),
            &self.token_scopes(successor.device_id).await?,
            config.session_ttl_secs,
        )?;
        tx.commit().await?;
        Ok(IssuedSession {
            tokens: ConnectSessionTokens {
                access_token,
                refresh_token: new_refresh,
                expires_in: config.session_ttl_secs,
                token_type: "Bearer".to_string(),
            },
            session: successor,
        })
    }

    /// Revokes a whole family.
    pub async fn revoke_family(&self, family_id: Uuid, reason: &str) -> Result<()> {
        sqlx::query(
            "UPDATE sessions SET revoked_at = now(), revoke_reason = $2
             WHERE family_id = $1 AND revoked_at IS NULL",
        )
        .bind(family_id)
        .bind(reason)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Loads and validates the session behind an access token.
    pub async fn authenticate(&self, claims: &AccessClaims) -> Result<Authenticated> {
        let session_id = Uuid::parse_str(&claims.session_id)
            .map_err(|_| AppError::unauthorized("invalid_token", "bad session id"))?;
        let device_id = Uuid::parse_str(&claims.device_id)
            .map_err(|_| AppError::unauthorized("invalid_token", "bad device id"))?;
        let user_id = Uuid::parse_str(&claims.sub)
            .map_err(|_| AppError::unauthorized("invalid_token", "bad user id"))?;
        let session = sqlx::query_as::<_, SessionRow>(
            "SELECT id, family_id, user_id, device_id, refresh_hash, access_jti, issued_at,
             expires_at, rotated_at, replaced_by, successor_secret, revoked_at, revoke_reason
             FROM sessions WHERE id = $1",
        )
        .bind(session_id)
        .fetch_optional(&self.pool)
        .await?
        .ok_or_else(|| AppError::unauthorized("invalid_token", "session no longer exists"))?;
        if session.revoked_at.is_some() {
            return Err(AppError::unauthorized(
                "session_revoked",
                "session was revoked",
            ));
        }
        if session.user_id != user_id
            || session.device_id != device_id
            || session.access_jti.to_string() != claims.jti
            || session.family_id.to_string() != claims.family_id
        {
            return Err(AppError::unauthorized(
                "invalid_token",
                "access token does not match its session",
            ));
        }
        let device = self.device(device_id).await?;
        if device.revoked_at.is_some() {
            return Err(AppError::unauthorized(
                "device_revoked",
                "device was revoked",
            ));
        }
        let user = self.user(user_id).await?;
        Ok(Authenticated {
            user,
            device,
            session,
        })
    }

    /// Records presence.
    pub async fn set_presence(
        &self,
        device_id: Uuid,
        user_id: Uuid,
        machine_id: Option<Uuid>,
        online: bool,
    ) -> Result<()> {
        sqlx::query(
            "INSERT INTO presence (device_id, user_id, machine_id, status, last_seen_at, updated_at)
             VALUES ($1, $2, $3, $4, now(), now())
             ON CONFLICT (device_id) DO UPDATE SET status = $4, machine_id = $3,
             last_seen_at = now(), updated_at = now()",
        )
        .bind(device_id)
        .bind(user_id)
        .bind(machine_id)
        .bind(if online { "online" } else { "offline" })
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Appends an audit entry. Metadata must never contain payloads or secrets.
    pub async fn audit(
        &self,
        user_id: Option<Uuid>,
        actor_device_id: Option<Uuid>,
        action: &str,
        target_type: Option<&str>,
        target_id: Option<&str>,
        metadata: serde_json::Value,
    ) -> Result<()> {
        sqlx::query(
            "INSERT INTO audit_log (user_id, actor_device_id, action, target_type, target_id, metadata)
             VALUES ($1, $2, $3, $4, $5, $6)",
        )
        .bind(user_id)
        .bind(actor_device_id)
        .bind(action)
        .bind(target_type)
        .bind(target_id)
        .bind(metadata)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Pages the audit log newest first.
    pub async fn audit_page(
        &self,
        user_id: Uuid,
        before_id: Option<i64>,
        limit: i64,
    ) -> Result<Vec<AuditRow>> {
        let limit = limit.clamp(1, 200);
        Ok(sqlx::query_as::<_, AuditRow>(
            "SELECT id, actor_device_id, action, target_type, target_id, metadata, at
             FROM audit_log
             WHERE user_id = $1 AND ($2::bigint IS NULL OR id < $2)
             ORDER BY id DESC LIMIT $3",
        )
        .bind(user_id)
        .bind(before_id)
        .bind(limit)
        .fetch_all(&self.pool)
        .await?)
    }

    /// Stores an envelope-encrypted secret, replacing any value with the same
    /// `(purpose, name)`.
    pub async fn put_secret(
        &self,
        user_id: Uuid,
        purpose: &str,
        name: &str,
        plaintext: &str,
        envelope: &Envelope,
    ) -> Result<()> {
        let aad = format!("{purpose}\0{name}");
        let sealed = envelope.seal_str(plaintext, aad.as_bytes())?;
        sqlx::query(
            "INSERT INTO encrypted_secrets
             (id, user_id, name, purpose, dek_nonce, wrapped_dek, nonce, ciphertext)
             VALUES ($1,$2,$3,$4,$5,$6,$7,$8)
             ON CONFLICT (user_id, purpose, name) DO UPDATE SET
             dek_nonce = $5, wrapped_dek = $6, nonce = $7, ciphertext = $8, updated_at = now()",
        )
        .bind(Uuid::now_v7())
        .bind(user_id)
        .bind(name)
        .bind(purpose)
        .bind(&sealed.dek_nonce)
        .bind(&sealed.wrapped_dek)
        .bind(&sealed.nonce)
        .bind(&sealed.ciphertext)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Reads an envelope-encrypted secret.
    pub async fn get_secret(
        &self,
        user_id: Uuid,
        purpose: &str,
        name: &str,
        envelope: &Envelope,
    ) -> Result<Option<String>> {
        let row: Option<SecretColumns> = sqlx::query_as(
            "SELECT dek_nonce, wrapped_dek, nonce, ciphertext FROM encrypted_secrets
             WHERE user_id = $1 AND purpose = $2 AND name = $3",
        )
        .bind(user_id)
        .bind(purpose)
        .bind(name)
        .fetch_optional(&self.pool)
        .await?;
        let Some((dek_nonce, wrapped_dek, nonce, ciphertext)) = row else {
            return Ok(None);
        };
        let sealed = SealedSecret {
            dek_nonce,
            wrapped_dek,
            nonce,
            ciphertext,
        };
        let aad = format!("{purpose}\0{name}");
        Ok(Some(envelope.open_str(&sealed, aad.as_bytes())?))
    }

    /// Deletes a secret.
    pub async fn delete_secret(&self, user_id: Uuid, purpose: &str, name: &str) -> Result<()> {
        sqlx::query(
            "DELETE FROM encrypted_secrets WHERE user_id = $1 AND purpose = $2 AND name = $3",
        )
        .bind(user_id)
        .bind(purpose)
        .bind(name)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Appends a usage row (Phase 5 sync target).
    pub async fn insert_usage(
        &self,
        user_id: Uuid,
        machine_id: Option<Uuid>,
        event: &UsageEvent,
    ) -> Result<i64> {
        let id: i64 = sqlx::query_scalar(
            "INSERT INTO usage_events
             (user_id, machine_id, thread_id, run_id, harness, model, provider, tokens_in,
              tokens_out, tokens_cached, cost_micros, ttft_ms, tps, ts, raw)
             VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14,$15) RETURNING id",
        )
        .bind(user_id)
        .bind(machine_id)
        .bind(&event.thread_id)
        .bind(&event.run_id)
        .bind(&event.harness)
        .bind(&event.model)
        .bind(&event.provider)
        .bind(event.tokens_in)
        .bind(event.tokens_out)
        .bind(event.tokens_cached)
        .bind(event.cost_micros)
        .bind(event.ttft_ms)
        .bind(event.tps)
        .bind(event.ts)
        .bind(&event.raw)
        .fetch_one(&self.pool)
        .await?;
        Ok(id)
    }

    /// Stores a ledger batch idempotently by `(user_id, row_id)`.
    pub async fn sync_usage(
        &self,
        user_id: Uuid,
        machine_id: Option<Uuid>,
        rows: &[UsageSyncRow],
    ) -> Result<UsageSyncResponse> {
        let mut response = UsageSyncResponse::default();
        let mut transaction = self.pool.begin().await?;
        for row in rows {
            let result = sqlx::query(
                "INSERT INTO usage_events
                 (user_id, machine_id, row_id, thread_id, run_id, workspace, machine, account,
                  harness, model, provider, credential_id, source, correlation_id, tokens_in,
                  tokens_out, tokens_cached, tokens_cached_creation, cost_micros, cost_source,
                  price_table, ttft_ms, tps, generation_ms, wall_ms, started_at, completed_at,
                  ts, raw, deduped, synced_at)
                 VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14,$15,$16,$17,$18,
                         $19,$20,$21,$22,$23,$24,$25,$26,$27,$28,$29,$30,now())
                 ON CONFLICT DO NOTHING",
            )
            .bind(user_id)
            .bind(machine_id)
            .bind(&row.row_id)
            .bind(&row.thread_id)
            .bind(&row.run_id)
            .bind(&row.workspace)
            .bind(&row.machine)
            .bind(&row.account)
            .bind(&row.harness)
            .bind(&row.model)
            .bind(&row.provider)
            .bind(&row.credential)
            .bind(provenance_label(row.source))
            .bind(&row.correlation_id)
            .bind(row.tokens_in as i64)
            .bind(row.tokens_out as i64)
            .bind(row.tokens_cached_read as i64)
            .bind(row.tokens_cached_creation as i64)
            .bind(row.cost_micros as i64)
            .bind(cost_source_label(row.cost_source))
            .bind(&row.price_table)
            .bind(row.ttft_ms.map(|value| value as i64))
            .bind(row.tps)
            .bind(row.generation_ms as i64)
            .bind(row.wall_ms as i64)
            .bind(row.started_at)
            .bind(row.completed_at)
            .bind(row.ts)
            .bind(&row.raw)
            .bind(row.deduped)
            .execute(&mut *transaction)
            .await?;
            if result.rows_affected() > 0 {
                response.accepted += 1;
            } else {
                response.duplicates += 1;
            }
        }
        transaction.commit().await?;
        Ok(response)
    }

    /// Aggregates the account's synced ledger.
    #[allow(clippy::too_many_arguments)]
    pub async fn aggregate_usage(
        &self,
        user_id: Uuid,
        scope: UsageScope,
        scope_id: Option<&str>,
        since: Option<DateTime<Utc>>,
        until: Option<DateTime<Utc>>,
        group_by: UsageGroupBy,
        bucket: Option<UsageBucket>,
    ) -> Result<UsageReport> {
        let (thread, workspace, machine) = match scope {
            UsageScope::Thread => (scope_id, None, None),
            UsageScope::Workspace => (None, scope_id, None),
            UsageScope::Machine => (None, None, scope_id),
            UsageScope::Global => (None, None, None),
        };
        let filter = "FROM usage_events
             WHERE user_id = $1
               AND NOT deduped
               AND ($2::text IS NULL OR thread_id = $2)
               AND ($3::text IS NULL OR workspace = $3)
               AND ($4::text IS NULL OR machine = $4)
               AND ($5::timestamptz IS NULL OR ts >= $5)
               AND ($6::timestamptz IS NULL OR ts < $6)";
        let query = format!(
            "SELECT
                COALESCE(SUM(tokens_in), 0)::bigint AS input_tokens,
                COALESCE(SUM(tokens_out), 0)::bigint AS output_tokens,
                COALESCE(SUM(tokens_cached), 0)::bigint AS cached_input_tokens,
                COALESCE(SUM(tokens_cached_creation), 0)::bigint AS cached_creation_tokens,
                COALESCE(SUM(cost_micros), 0)::bigint AS cost_micros,
                COALESCE(AVG(ttft_ms) FILTER (WHERE ttft_ms IS NOT NULL), 0)::float8 AS ttft_ms,
                COALESCE(AVG(tps) FILTER (WHERE tps IS NOT NULL), 0)::float8 AS tps,
                COALESCE(SUM(wall_ms), 0)::bigint AS wall_ms,
                COALESCE(SUM(generation_ms), 0)::bigint AS generation_ms,
                COUNT(*)::bigint AS turns,
                MIN(COALESCE(started_at, ts)) AS first_at,
                MAX(COALESCE(completed_at, ts)) AS last_at
             {filter}"
        );
        let total = sqlx::query(&query)
            .bind(user_id)
            .bind(thread)
            .bind(workspace)
            .bind(machine)
            .bind(since)
            .bind(until)
            .fetch_one(&self.pool)
            .await?;
        let totals = totals_from_pg(&total);

        let mut groups = Vec::new();
        if let Some(expression) = group_expression(group_by) {
            let query = format!(
                "SELECT {expression} AS group_key,
                    COALESCE(SUM(tokens_in), 0)::bigint AS input_tokens,
                    COALESCE(SUM(tokens_out), 0)::bigint AS output_tokens,
                    COALESCE(SUM(tokens_cached), 0)::bigint AS cached_input_tokens,
                    COALESCE(SUM(tokens_cached_creation), 0)::bigint AS cached_creation_tokens,
                    COALESCE(SUM(cost_micros), 0)::bigint AS cost_micros,
                    COALESCE(AVG(ttft_ms) FILTER (WHERE ttft_ms IS NOT NULL), 0)::float8 AS ttft_ms,
                    COALESCE(AVG(tps) FILTER (WHERE tps IS NOT NULL), 0)::float8 AS tps,
                    COALESCE(SUM(wall_ms), 0)::bigint AS wall_ms,
                    COALESCE(SUM(generation_ms), 0)::bigint AS generation_ms,
                    COUNT(*)::bigint AS turns,
                    MIN(COALESCE(started_at, ts)) AS first_at,
                    MAX(COALESCE(completed_at, ts)) AS last_at
                 {filter}
                 GROUP BY group_key
                 ORDER BY group_key ASC"
            );
            let rows = sqlx::query(&query)
                .bind(user_id)
                .bind(thread)
                .bind(workspace)
                .bind(machine)
                .bind(since)
                .bind(until)
                .fetch_all(&self.pool)
                .await?;
            groups = rows
                .iter()
                .map(|row| UsageGroup {
                    key: row.try_get::<String, _>("group_key").unwrap_or_default(),
                    totals: totals_from_pg(row),
                })
                .collect();
        }

        let mut buckets = Vec::new();
        if let Some(width) = bucket {
            let unit = match width {
                UsageBucket::Hour => "hour",
                UsageBucket::Day => "day",
            };
            let query = format!(
                "SELECT date_trunc($7, COALESCE(ts, completed_at)) AS bucket_at,
                    COALESCE(SUM(tokens_in), 0)::bigint AS input_tokens,
                    COALESCE(SUM(tokens_out), 0)::bigint AS output_tokens,
                    COALESCE(SUM(tokens_cached), 0)::bigint AS cached_input_tokens,
                    COALESCE(SUM(tokens_cached_creation), 0)::bigint AS cached_creation_tokens,
                    COALESCE(SUM(cost_micros), 0)::bigint AS cost_micros,
                    COALESCE(AVG(ttft_ms) FILTER (WHERE ttft_ms IS NOT NULL), 0)::float8 AS ttft_ms,
                    COALESCE(AVG(tps) FILTER (WHERE tps IS NOT NULL), 0)::float8 AS tps,
                    COALESCE(SUM(wall_ms), 0)::bigint AS wall_ms,
                    COALESCE(SUM(generation_ms), 0)::bigint AS generation_ms,
                    COUNT(*)::bigint AS turns,
                    MIN(COALESCE(started_at, ts)) AS first_at,
                    MAX(COALESCE(completed_at, ts)) AS last_at
                 {filter}
                 GROUP BY bucket_at
                 ORDER BY bucket_at ASC"
            );
            let rows = sqlx::query(&query)
                .bind(user_id)
                .bind(thread)
                .bind(workspace)
                .bind(machine)
                .bind(since)
                .bind(until)
                .bind(unit)
                .fetch_all(&self.pool)
                .await?;
            buckets = rows
                .iter()
                .filter_map(|row| {
                    let start: DateTime<Utc> = row.try_get("bucket_at").ok()?;
                    let end = match width {
                        UsageBucket::Hour => start + Duration::hours(1),
                        UsageBucket::Day => start + Duration::days(1),
                    };
                    Some(UsageBucketPoint {
                        start,
                        end,
                        totals: totals_from_pg(row),
                    })
                })
                .collect();
        }

        Ok(UsageReport {
            scope,
            scope_id: scope_id.map(str::to_owned),
            range: h4rn_protocol::UsageRange { since, until },
            group_by,
            bucket,
            series_group_by: None,
            totals,
            buckets,
            series: Vec::new(),
            groups,
        })
    }
}

fn group_expression(group_by: UsageGroupBy) -> Option<&'static str> {
    match group_by {
        UsageGroupBy::None => None,
        UsageGroupBy::Thread => Some("COALESCE(thread_id, 'unscoped')"),
        UsageGroupBy::Workspace => Some("COALESCE(workspace, 'unscoped')"),
        UsageGroupBy::Machine => Some("COALESCE(machine, 'unscoped')"),
        UsageGroupBy::Harness => Some("COALESCE(harness, 'unknown')"),
        UsageGroupBy::Model => Some("COALESCE(model, 'unknown')"),
    }
}

fn totals_from_pg(row: &sqlx::postgres::PgRow) -> UsageTotals {
    let wall_ms = row.try_get::<i64, _>("wall_ms").unwrap_or(0).max(0) as u64;
    let first_at = row
        .try_get::<Option<DateTime<Utc>>, _>("first_at")
        .ok()
        .flatten();
    let last_at = row
        .try_get::<Option<DateTime<Utc>>, _>("last_at")
        .ok()
        .flatten();
    let runtime_ms = match (first_at, last_at) {
        (Some(first), Some(last)) => (last - first).num_milliseconds().max(0) as u64,
        _ => wall_ms,
    };
    UsageTotals {
        input_tokens: row.try_get::<i64, _>("input_tokens").unwrap_or(0).max(0) as u64,
        output_tokens: row.try_get::<i64, _>("output_tokens").unwrap_or(0).max(0) as u64,
        cached_input_tokens: row
            .try_get::<i64, _>("cached_input_tokens")
            .unwrap_or(0)
            .max(0) as u64,
        cached_creation_tokens: row
            .try_get::<i64, _>("cached_creation_tokens")
            .unwrap_or(0)
            .max(0) as u64,
        cost_micros: row.try_get::<i64, _>("cost_micros").unwrap_or(0).max(0) as u64,
        ttft_ms: row.try_get::<f64, _>("ttft_ms").unwrap_or(0.0).max(0.0) as u64,
        tps: row.try_get::<f64, _>("tps").unwrap_or(0.0),
        wall_ms,
        generation_ms: row.try_get::<i64, _>("generation_ms").unwrap_or(0).max(0) as u64,
        active_ms: wall_ms,
        idle_ms: runtime_ms.saturating_sub(wall_ms),
        runtime_ms,
        turns: row.try_get::<i64, _>("turns").unwrap_or(0).max(0) as u64,
    }
}

fn provenance_label(source: UsageProvenance) -> &'static str {
    match source {
        UsageProvenance::SelfMetered => "self_metered",
        UsageProvenance::ProxyReported => "proxy_reported",
    }
}

fn cost_source_label(source: CostSource) -> &'static str {
    match source {
        CostSource::HarnessReported => "harness_reported",
        CostSource::PriceTable => "price_table",
        CostSource::ProxyReported => "proxy_reported",
        CostSource::Unknown => "unknown",
    }
}

/// One stored skill sync entry, including its sealed bundle.
#[derive(Clone, Debug, FromRow)]
pub struct SkillEntryRow {
    /// Owner.
    pub user_id: Uuid,
    /// Registry id.
    pub skill_id: String,
    /// Skill name.
    pub name: String,
    /// Skill description.
    pub description: String,
    /// Content version hash.
    pub version: String,
    /// Serialised version vector.
    pub vector_json: String,
    /// ETag of this entry.
    pub etag: String,
    /// Tombstone flag.
    pub deleted: bool,
    /// Envelope nonce wrapping the DEK.
    pub dek_nonce: Vec<u8>,
    /// Wrapped DEK.
    pub wrapped_dek: Vec<u8>,
    /// Payload nonce.
    pub nonce: Vec<u8>,
    /// Encrypted bundle payload.
    pub ciphertext: Vec<u8>,
    /// Sender's wall clock.
    pub updated_at: DateTime<Utc>,
    /// Server receive time, for diagnostics only.
    pub server_updated_at: DateTime<Utc>,
    /// Commit-ordered per-user sequence, used as the pull cursor.
    pub sync_seq: i64,
}

/// Input for [`Store::upsert_skill_entry`].
#[derive(Clone, Debug)]
pub struct NewSkillEntry<'a> {
    /// Registry id.
    pub skill_id: &'a str,
    /// Skill name.
    pub name: &'a str,
    /// Skill description.
    pub description: &'a str,
    /// Content version.
    pub version: &'a str,
    /// Serialised version vector.
    pub vector_json: &'a str,
    /// ETag computed by the handler.
    pub etag: &'a str,
    /// Tombstone flag.
    pub deleted: bool,
    /// Sealed bundle.
    pub sealed: SealedSecret,
    /// Sender's wall clock.
    pub updated_at: DateTime<Utc>,
}

/// One sync conflict row.
#[derive(Clone, Debug, FromRow)]
pub struct SkillConflictRow {
    /// Row id.
    pub id: i64,
    /// Registry id.
    pub skill_id: String,
    /// Version kept by the server.
    pub local_version: Option<String>,
    /// Version that lost.
    pub remote_version: String,
    /// Why it lost.
    pub reason: String,
    /// Detection time.
    pub created_at: DateTime<Utc>,
}

impl Store {
    /// One stored skill entry.
    pub async fn skill_entry(
        &self,
        user_id: Uuid,
        skill_id: &str,
    ) -> Result<Option<SkillEntryRow>> {
        sqlx::query_as::<_, SkillEntryRow>(
            "SELECT user_id, skill_id, name, description, version, vector_json, etag,
                    deleted, dek_nonce, wrapped_dek, nonce, ciphertext, updated_at,
                    server_updated_at, sync_seq
               FROM skill_sync_entries WHERE user_id = $1 AND skill_id = $2",
        )
        .bind(user_id)
        .bind(skill_id)
        .fetch_optional(&self.pool)
        .await
        .map_err(Into::into)
    }

    /// Every entry after `since` in commit order, or all entries when `since`
    /// is `None`.
    pub async fn skill_entries_since(
        &self,
        user_id: Uuid,
        since: Option<i64>,
    ) -> Result<Vec<SkillEntryRow>> {
        sqlx::query_as::<_, SkillEntryRow>(
            "SELECT user_id, skill_id, name, description, version, vector_json, etag,
                    deleted, dek_nonce, wrapped_dek, nonce, ciphertext, updated_at,
                    server_updated_at, sync_seq
               FROM skill_sync_entries
              WHERE user_id = $1 AND ($2::bigint IS NULL OR sync_seq > $2)
              ORDER BY sync_seq",
        )
        .bind(user_id)
        .bind(since)
        .fetch_all(&self.pool)
        .await
        .map_err(Into::into)
    }

    /// Inserts or replaces one entry in its own transaction.
    pub async fn upsert_skill_entry(
        &self,
        user_id: Uuid,
        device_id: Option<Uuid>,
        entry: &NewSkillEntry<'_>,
    ) -> Result<()> {
        let mut tx = self.pool.begin().await?;
        Self::upsert_skill_entry_in(&mut tx, user_id, device_id, entry).await?;
        tx.commit().await?;
        Ok(())
    }

    /// Inserts or replaces one entry inside `tx`.
    ///
    /// The entry's cursor is a per-user sequence taken under the user's clock
    /// row lock. Because that lock is held until the transaction commits,
    /// sequence order is commit order: a pull can never observe a later
    /// sequence while an earlier one is still uncommitted, which is what makes
    /// the cursor safe under concurrent pushes.
    pub async fn upsert_skill_entry_in(
        tx: &mut Transaction<'_, Postgres>,
        user_id: Uuid,
        device_id: Option<Uuid>,
        entry: &NewSkillEntry<'_>,
    ) -> Result<()> {
        let sync_seq = sqlx::query_scalar::<_, i64>(
            "INSERT INTO skill_sync_clock (user_id, last_seq) VALUES ($1, 1)
             ON CONFLICT (user_id) DO UPDATE SET last_seq = skill_sync_clock.last_seq + 1
             RETURNING last_seq",
        )
        .bind(user_id)
        .fetch_one(&mut **tx)
        .await?;

        sqlx::query(
            "INSERT INTO skill_sync_entries
                (user_id, skill_id, name, description, version, vector_json, etag, deleted,
                 dek_nonce, wrapped_dek, nonce, ciphertext, updated_at, server_updated_at,
                 updated_by_device, sync_seq)
             VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13, now(), $14, $15)
             ON CONFLICT(user_id, skill_id) DO UPDATE SET
                name = excluded.name,
                description = excluded.description,
                version = excluded.version,
                vector_json = excluded.vector_json,
                etag = excluded.etag,
                deleted = excluded.deleted,
                dek_nonce = excluded.dek_nonce,
                wrapped_dek = excluded.wrapped_dek,
                nonce = excluded.nonce,
                ciphertext = excluded.ciphertext,
                updated_at = excluded.updated_at,
                server_updated_at = now(),
                updated_by_device = excluded.updated_by_device,
                sync_seq = excluded.sync_seq",
        )
        .bind(user_id)
        .bind(entry.skill_id)
        .bind(entry.name)
        .bind(entry.description)
        .bind(entry.version)
        .bind(entry.vector_json)
        .bind(entry.etag)
        .bind(entry.deleted)
        .bind(&entry.sealed.dek_nonce)
        .bind(&entry.sealed.wrapped_dek)
        .bind(&entry.sealed.nonce)
        .bind(&entry.sealed.ciphertext)
        .bind(entry.updated_at)
        .bind(device_id)
        .bind(sync_seq)
        .execute(&mut **tx)
        .await?;
        Ok(())
    }

    /// Unresolved conflicts for one user.
    pub async fn skill_conflicts(&self, user_id: Uuid) -> Result<Vec<SkillConflictRow>> {
        sqlx::query_as::<_, SkillConflictRow>(
            "SELECT id, skill_id, local_version, remote_version, reason, created_at
               FROM skill_sync_conflicts
              WHERE user_id = $1 AND resolved_at IS NULL
              ORDER BY created_at DESC, id DESC",
        )
        .bind(user_id)
        .fetch_all(&self.pool)
        .await
        .map_err(Into::into)
    }

    /// Records one sync conflict.
    pub async fn record_skill_conflict(
        &self,
        user_id: Uuid,
        skill_id: &str,
        local_version: Option<&str>,
        remote_version: &str,
        reason: &str,
    ) -> Result<()> {
        sqlx::query(
            "INSERT INTO skill_sync_conflicts
                (user_id, skill_id, local_version, remote_version, reason)
             VALUES ($1,$2,$3,$4,$5)",
        )
        .bind(user_id)
        .bind(skill_id)
        .bind(local_version)
        .bind(remote_version)
        .bind(reason)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Reads the entries changed since `since` together with the cursor the
    /// client must resume from.
    ///
    /// The cursor is derived from the rows returned, never from a second query:
    /// a push that commits between two reads would otherwise advance the
    /// cursor past a row this pull did not include, and the client would skip
    /// it forever. With no rows the cursor is `since` unchanged.
    pub async fn skill_pull(
        &self,
        user_id: Uuid,
        since: Option<i64>,
    ) -> Result<(Vec<SkillEntryRow>, Option<i64>)> {
        let rows = self.skill_entries_since(user_id, since).await?;
        let cursor = rows.iter().map(|row| row.sync_seq).max().or(since);
        Ok((rows, cursor))
    }

    /// The newest commit-ordered sequence, used as the pull cursor.
    pub async fn skill_cursor(&self, user_id: Uuid) -> Result<Option<i64>> {
        sqlx::query_scalar::<_, Option<i64>>(
            "SELECT MAX(sync_seq) FROM skill_sync_entries WHERE user_id = $1",
        )
        .bind(user_id)
        .fetch_one(&self.pool)
        .await
        .map_err(Into::into)
    }
}

/// Inserts or reuses a device within a transaction.
async fn upsert_device_in(
    tx: &mut Transaction<'_, Postgres>,
    user_id: Uuid,
    new_device: &NewDevice,
) -> Result<DeviceRow> {
    let existing = sqlx::query_as::<_, DeviceRow>(
        "SELECT id, user_id, machine_id, kind, name, proof_public_key,
         noise_static_public_key, platform, last_seen, revoked_at, created_at
         FROM devices WHERE proof_public_key = $1 FOR UPDATE",
    )
    .bind(&new_device.proof_public_key)
    .fetch_optional(&mut **tx)
    .await?;

    if let Some(device) = existing {
        if device.user_id != user_id {
            return Err(AppError::forbidden(
                "device_key_claimed",
                "device key belongs to another account",
            ));
        }
        if device.revoked_at.is_some() {
            return Err(AppError::forbidden(
                "device_revoked",
                "device was revoked; register a new key",
            ));
        }
        return sqlx::query_as::<_, DeviceRow>(
            "UPDATE devices SET name = $2, kind = $3,
             noise_static_public_key = COALESCE($4, noise_static_public_key),
             platform = COALESCE($5, platform), last_seen = now()
             WHERE id = $1
             RETURNING id, user_id, machine_id, kind, name, proof_public_key,
             noise_static_public_key, platform, last_seen, revoked_at, created_at",
        )
        .bind(device.id)
        .bind(&new_device.name)
        .bind(new_device.kind.as_str())
        .bind(&new_device.noise_static_public_key)
        .bind(&new_device.platform)
        .fetch_one(&mut **tx)
        .await
        .map_err(Into::into);
    }

    let id = Uuid::now_v7();
    sqlx::query_as::<_, DeviceRow>(
        "INSERT INTO devices
         (id, user_id, kind, name, proof_public_key, noise_static_public_key, platform, last_seen)
         VALUES ($1,$2,$3,$4,$5,$6,$7, now())
         RETURNING id, user_id, machine_id, kind, name, proof_public_key,
         noise_static_public_key, platform, last_seen, revoked_at, created_at",
    )
    .bind(id)
    .bind(user_id)
    .bind(new_device.kind.as_str())
    .bind(&new_device.name)
    .bind(&new_device.proof_public_key)
    .bind(&new_device.noise_static_public_key)
    .bind(&new_device.platform)
    .fetch_one(&mut **tx)
    .await
    .map_err(Into::into)
}

/// Creates or updates the machine owned by a daemon device.
async fn attach_daemon_machine(
    tx: &mut Transaction<'_, Postgres>,
    user_id: Uuid,
    device: &DeviceRow,
    new_device: &NewDevice,
) -> Result<()> {
    let machine_name = new_device
        .machine_name
        .as_deref()
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .unwrap_or(&new_device.name);

    let existing: Option<(Uuid, Option<Uuid>)> = sqlx::query_as(
        "SELECT m.id, m.daemon_device_id FROM machines m
         WHERE m.user_id = $1 AND (m.daemon_device_id = $2 OR m.name = $3)
         ORDER BY (m.daemon_device_id = $2) DESC LIMIT 1 FOR UPDATE",
    )
    .bind(user_id)
    .bind(device.id)
    .bind(machine_name)
    .fetch_optional(&mut **tx)
    .await?;

    let machine_id = match existing {
        Some((id, _)) => {
            sqlx::query(
                "UPDATE machines SET daemon_device_id = $2, name = $3, updated_at = now()
                 WHERE id = $1",
            )
            .bind(id)
            .bind(device.id)
            .bind(machine_name)
            .execute(&mut **tx)
            .await?;
            id
        }
        None => {
            let id = Uuid::now_v7();
            sqlx::query(
                "INSERT INTO machines (id, user_id, name, daemon_device_id)
                 VALUES ($1,$2,$3,$4)",
            )
            .bind(id)
            .bind(user_id)
            .bind(machine_name)
            .bind(device.id)
            .execute(&mut **tx)
            .await?;
            id
        }
    };

    sqlx::query("UPDATE devices SET machine_id = $2 WHERE id = $1")
        .bind(device.id)
        .bind(machine_id)
        .execute(&mut **tx)
        .await?;

    let host_name = new_device
        .platform
        .clone()
        .unwrap_or_else(|| "host".to_string());
    sqlx::query(
        "INSERT INTO machine_hosts (id, machine_id, name, kind)
         VALUES ($1,$2,$3,'host') ON CONFLICT (machine_id, name) DO NOTHING",
    )
    .bind(Uuid::now_v7())
    .bind(machine_id)
    .bind(host_name)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

/// Builds a presence update.
pub fn presence_update(
    machine_id: Uuid,
    online: bool,
    last_seen: Option<DateTime<Utc>>,
) -> PresenceUpdate {
    PresenceUpdate {
        machine_id: machine_id.to_string(),
        online,
        last_seen,
    }
}

/// Builds a user wire type.
pub fn user_to_wire(user: &UserRow) -> ConnectUser {
    ConnectUser {
        id: user.id.to_string(),
        email: user.email.clone(),
        display_name: user.display_name.clone(),
        avatar_url: user.avatar_url.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reuse_inside_the_grace_window_replays_the_successor() {
        let now = Utc::now();
        assert_eq!(
            classify_reuse(None, now, Duration::seconds(30)),
            ReuseDecision::Rotate
        );
        assert_eq!(
            classify_reuse(Some(now - Duration::seconds(5)), now, Duration::seconds(30)),
            ReuseDecision::ReplaySuccessor
        );
        assert_eq!(
            classify_reuse(
                Some(now - Duration::seconds(31)),
                now,
                Duration::seconds(30)
            ),
            ReuseDecision::RevokeFamily
        );
    }
}
