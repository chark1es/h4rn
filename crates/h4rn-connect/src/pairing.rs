//! Pairing: the `h4rn://pair?...` QR and its single-use claim (ADR-010).
//!
//! A daemon creates a pairing for its machine. The relay stores only a BLAKE3
//! digest of the one-time secret, embeds the daemon's Noise static public key
//! in the QR, and expires the record after five minutes. Claiming it is a
//! single transaction: the QR becomes dead, the claiming device is linked to
//! the machine, and the daemon is told over the relay.
//!
//! The relay is not part of the trust chain. The Noise handshake authenticates
//! the daemon with the key from the QR and the client with the key it registered
//! at login, so a relay that lies about a pairing still cannot impersonate
//! either end.

use chrono::Utc;
use h4rn_protocol::{
    MachineSummary, PairScope, PairingClaimCodeRequest, PairingClaimRequest, PairingClaimResponse,
    PairingCreateResponse, PairingPayload, normalize_short_code,
};
use uuid::Uuid;

use crate::api::{AppState, Authed};
use crate::crypto::{b64, b64_decode, hash_pairing_code, hash_pairing_secret, random_b64};
use crate::error::{AppError, Result};

/// Creates a pairing and renders its QR payload.
pub async fn create(
    state: &AppState,
    authed: &Authed,
    machine_id: Uuid,
    scopes: Option<Vec<PairScope>>,
) -> Result<PairingCreateResponse> {
    // Handing out scopes is itself an admin act: a device cannot mint a
    // pairing more powerful than the one it holds *on this machine*.
    let scopes = PairScope::normalize(scopes.unwrap_or_else(|| PairScope::DEFAULT.to_vec()));
    let machine = state.store.machine(machine_id).await?;
    if machine.user_id != authed.user.id {
        return Err(AppError::forbidden(
            "machine_forbidden",
            "machine belongs to another user",
        ));
    }
    authed
        .require_on(state, machine_id, PairScope::Admin)
        .await?;
    let daemon_device_id = machine.daemon_device_id.ok_or_else(|| {
        AppError::conflict(
            "machine_has_no_daemon",
            "the machine has no daemon to pair with",
        )
    })?;
    let daemon = state.store.device(daemon_device_id).await?;
    let noise_key = daemon
        .noise_static_public_key
        .as_deref()
        .map(b64)
        .ok_or_else(|| {
            AppError::conflict(
                "daemon_key_missing",
                "the daemon did not register a Noise static public key",
            )
        })?;

    let secret = random_b64(32);
    let (pairing, short_code) = state
        .store
        .create_pairing(
            machine_id,
            authed.device.id,
            &hash_pairing_secret(&secret),
            &scopes,
            state.config.pairing_ttl(),
        )
        .await?;

    let payload = PairingPayload {
        connect_url: state.config.public_base_url.clone(),
        machine_id: machine_id.to_string(),
        daemon_noise_public_key: noise_key,
        secret,
        expires_at: pairing.expires_at,
        scopes: crate::store::parse_scopes(&pairing.scopes),
    };
    let qr = payload.to_url();

    state
        .store
        .audit(
            Some(authed.user.id),
            Some(authed.device.id),
            "pairing.created",
            Some("machine"),
            Some(&machine_id.to_string()),
            serde_json::json!({
                "pairing_id": pairing.id.to_string(),
                "scopes": pairing.scopes,
            }),
        )
        .await
        .ok();

    Ok(PairingCreateResponse {
        pairing: pairing.to_wire(Utc::now()),
        qr,
        short_code,
    })
}

/// Claims a scanned QR for the authenticated device.
pub async fn claim(
    state: &AppState,
    authed: &Authed,
    request: PairingClaimRequest,
) -> Result<PairingClaimResponse> {
    let payload = PairingPayload::parse(&request.payload).map_err(|error| {
        AppError::bad_request_hint(
            "pairing_invalid",
            error.to_string(),
            "Scan the QR code shown on the daemon.",
        )
    })?;

    let configured = state.config.public_base_url.trim_end_matches('/');
    if payload.connect_url.trim_end_matches('/') != configured {
        return Err(AppError::bad_request_hint(
            "pairing_wrong_relay",
            "the pairing was created for a different relay",
            format!("This device is signed in to {configured}."),
        ));
    }
    if payload.expires_at <= Utc::now() {
        return Err(AppError::bad_request_hint(
            "pairing_expired",
            "the pairing code has expired",
            "Generate a new pairing on the daemon.",
        ));
    }
    let machine_id = Uuid::parse_str(&payload.machine_id).map_err(|_| {
        AppError::bad_request_hint(
            "pairing_invalid",
            "pairing machine id is invalid",
            "Scan a fresh QR code.",
        )
    })?;

    let supplied_key = b64_decode(&request.device_public_key).ok_or_else(|| {
        AppError::bad_request("device_key_invalid", "device_public_key is not base64")
    })?;
    if supplied_key != authed.device.proof_public_key {
        return Err(AppError::forbidden(
            "device_key_mismatch",
            "device_public_key does not match the authenticated device",
        ));
    }
    let noise_key = match request.noise_static_public_key.as_deref() {
        Some(value) => Some(b64_decode(value).ok_or_else(|| {
            AppError::bad_request(
                "device_key_invalid",
                "noise_static_public_key is not base64",
            )
        })?),
        None => None,
    };
    if let Some(key) = noise_key {
        state
            .store
            .set_device_noise_key(authed.device.id, &key)
            .await?;
    }

    let (pairing, machine) = state
        .store
        .claim_pairing(
            machine_id,
            authed.user.id,
            authed.device.id,
            &payload.secret,
            state.config.rate_limits.pairing_attempts as i32,
        )
        .await?;
    finish(
        state,
        authed,
        request.device_name.as_deref(),
        pairing,
        machine,
    )
    .await
}

/// Claims a pairing by its typed short code for the authenticated device.
pub async fn claim_by_code(
    state: &AppState,
    authed: &Authed,
    request: PairingClaimCodeRequest,
) -> Result<PairingClaimResponse> {
    let code = normalize_short_code(&request.code).ok_or_else(|| {
        AppError::bad_request_hint(
            "pairing_code_invalid",
            "the pairing code must be 8 letters and digits",
            "Type the code exactly as the daemon shows it.",
        )
    })?;
    let supplied_key = b64_decode(&request.device_public_key).ok_or_else(|| {
        AppError::bad_request("device_key_invalid", "device_public_key is not base64")
    })?;
    if supplied_key != authed.device.proof_public_key {
        return Err(AppError::forbidden(
            "device_key_mismatch",
            "device_public_key does not match the authenticated device",
        ));
    }
    if let Some(value) = request.noise_static_public_key.as_deref() {
        let key = b64_decode(value).ok_or_else(|| {
            AppError::bad_request(
                "device_key_invalid",
                "noise_static_public_key is not base64",
            )
        })?;
        state
            .store
            .set_device_noise_key(authed.device.id, &key)
            .await?;
    }
    let (pairing, machine) = state
        .store
        .claim_pairing_by_code(authed.user.id, authed.device.id, &hash_pairing_code(&code))
        .await?;
    finish(
        state,
        authed,
        request.device_name.as_deref(),
        pairing,
        machine,
    )
    .await
}

/// Shared tail of both claims: name the device, audit and tell the daemon.
async fn finish(
    state: &AppState,
    authed: &Authed,
    device_name: Option<&str>,
    pairing: crate::store::PairingRow,
    machine: crate::store::MachineRow,
) -> Result<PairingClaimResponse> {
    let machine_id = machine.id;
    if let Some(name) = device_name {
        state
            .store
            .rename_device(authed.user.id, authed.device.id, name)
            .await?;
    }

    let device = state.store.device(authed.device.id).await?;
    let summary = machine_summary(state, &machine).await?;
    let daemon_noise_public_key = daemon_noise_key(state, &machine).await?;

    state
        .store
        .audit(
            Some(authed.user.id),
            Some(authed.device.id),
            "device.paired",
            Some("machine"),
            Some(&machine_id.to_string()),
            serde_json::json!({
                "pairing_id": pairing.id.to_string(),
                "scopes": pairing.scopes,
            }),
        )
        .await
        .ok();

    let mut wire = device.to_wire();
    wire.scopes = crate::store::parse_scopes(&pairing.scopes);
    state.relay.notify_paired(machine_id, wire).await;

    Ok(PairingClaimResponse {
        pairing: pairing.to_wire(Utc::now()),
        machine: summary,
        daemon_noise_public_key,
    })
}

async fn daemon_noise_key(state: &AppState, machine: &crate::store::MachineRow) -> Result<String> {
    let daemon_device_id = machine.daemon_device_id.ok_or_else(|| {
        AppError::conflict(
            "machine_has_no_daemon",
            "the machine has no daemon to pair with",
        )
    })?;
    state
        .store
        .device(daemon_device_id)
        .await?
        .noise_static_public_key
        .as_deref()
        .map(b64)
        .ok_or_else(|| {
            AppError::conflict(
                "daemon_key_missing",
                "the daemon did not register a Noise static public key",
            )
        })
}

/// Builds the consistent machine representation used by every endpoint.
pub async fn machine_summary(
    state: &AppState,
    machine: &crate::store::MachineRow,
) -> Result<MachineSummary> {
    let hosts = state.store.hosts(machine.id).await?;
    let last_seen = match machine.daemon_device_id {
        Some(device_id) => state
            .store
            .device(device_id)
            .await
            .ok()
            .and_then(|device| device.last_seen),
        None => None,
    };
    Ok(MachineSummary {
        id: machine.id.to_string(),
        name: machine.name.clone(),
        online: state.relay.machine_online(machine.id).await,
        last_seen,
        daemon_device_id: machine.daemon_device_id.map(|id| id.to_string()),
        hosts: hosts.iter().map(|host| host.to_wire()).collect(),
        addresses: state.store.addresses(machine.id).await?,
    })
}
