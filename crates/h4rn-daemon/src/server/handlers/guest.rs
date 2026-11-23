//! Guest-link requests from the owner (ADR-044).
//!
//! The guest itself never sends a `ClientRequest`: it has no device identity.
//! These are the owner's controls, and every one is scoped by the thread it
//! names. The base URL is chosen here so the link is honest about where it
//! points: the Connect relay when this machine is linked, otherwise the
//! daemon's own network address.

use h4rn_protocol::{ClientRequest, GuestSettings, ProtoError, ResponseBody};
use tokio::task::JoinHandle;

use crate::server::Connection;

/// Requests owned by this service.
pub(super) fn matches(request: &ClientRequest) -> bool {
    matches!(
        request,
        ClientRequest::GetGuestSharing { .. }
            | ClientRequest::ShareThreadWithGuest { .. }
            | ClientRequest::StopGuestSharing { .. }
            | ClientRequest::SendGuestRequest { .. }
            | ClientRequest::DismissGuestRequest { .. }
            | ClientRequest::GetGuestSettings
            | ClientRequest::SetGuestSettings { .. }
    )
}

/// Handles one owner-side guest request.
pub(super) async fn handle(
    conn: &Connection,
    request: ClientRequest,
) -> Result<ResponseBody, ProtoError> {
    match request {
        ClientRequest::GetGuestSharing { thread_id } => {
            let base = share_base(conn);
            let sharing = conn
                .state
                .guest
                .sharing(&thread_id, &base)
                .await
                .map_err(internal)?;
            Ok(ResponseBody::GuestSharing { sharing })
        }
        ClientRequest::ShareThreadWithGuest { thread_id } => {
            let base = share_base(conn);
            let created = conn
                .state
                .guest
                .create(&thread_id, &base)
                .await
                .map_err(|error| match error {
                    crate::guest::GuestError::Invalid(message) => ProtoError::new(
                        h4rn_protocol::ProtoErrorCode::InvalidRequest,
                        message,
                        "Turn on Settings → Labs → Guest links, then try again",
                    ),
                    crate::guest::GuestError::NotFound(message) => ProtoError::new(
                        h4rn_protocol::ProtoErrorCode::NotFound,
                        message,
                        "Refresh the thread list and try again",
                    ),
                    other => internal(other),
                })?;
            conn.audit(
                "guest.link_created",
                Some("thread"),
                Some(thread_id.as_ref()),
                serde_json::json!({"expires_at": created.link.expires_at}),
            );
            Ok(ResponseBody::GuestSharing {
                sharing: h4rn_protocol::GuestSharing {
                    thread_id,
                    links_enabled: true,
                    link: Some(created.link),
                },
            })
        }
        ClientRequest::StopGuestSharing { thread_id } => {
            conn.state
                .guest
                .stop_sharing(&thread_id)
                .await
                .map_err(internal)?;
            conn.audit(
                "guest.link_stopped",
                Some("thread"),
                Some(thread_id.as_ref()),
                serde_json::json!({}),
            );
            Ok(ResponseBody::Ack)
        }
        ClientRequest::SendGuestRequest {
            thread_id,
            request_id,
        } => {
            conn.state
                .guest
                .send_request(&thread_id, &request_id)
                .await
                .map_err(guest_error)?;
            conn.audit(
                "guest.request_sent",
                Some("thread"),
                Some(thread_id.as_ref()),
                serde_json::json!({"request_id": request_id}),
            );
            // The turn's own events carry the accepted message; this answer
            // only says the request was consumed.
            Ok(ResponseBody::Ack)
        }
        ClientRequest::DismissGuestRequest {
            thread_id,
            request_id,
        } => {
            conn.state
                .guest
                .dismiss_request(&thread_id, &request_id)
                .await
                .map_err(guest_error)?;
            conn.audit(
                "guest.request_dismissed",
                Some("thread"),
                Some(thread_id.as_ref()),
                serde_json::json!({"request_id": request_id}),
            );
            Ok(ResponseBody::Ack)
        }
        ClientRequest::GetGuestSettings => {
            let enabled = conn.state.guest.enabled().await.map_err(internal)?;
            Ok(ResponseBody::GuestSettings {
                settings: GuestSettings { enabled },
            })
        }
        ClientRequest::SetGuestSettings { enabled } => {
            let settings = conn
                .state
                .guest
                .set_enabled(enabled)
                .await
                .map_err(internal)?;
            conn.audit(
                "guest.setting_changed",
                Some("setting"),
                Some("guest_links"),
                serde_json::json!({"enabled": enabled}),
            );
            Ok(ResponseBody::GuestSettings { settings })
        }
        other => Err(crate::server::not_implemented(&other)),
    }
}

/// Spawns this service's transient-event forwarder.
///
/// Guest requests arrive through the thread's journal like every other event,
/// so a connection has nothing extra to follow.
pub(super) fn spawn(_conn: &Connection) -> JoinHandle<()> {
    tokio::spawn(async {})
}

/// Where a guest link points.
///
/// A linked machine advertises the relay address, so the guest reaches it the
/// same way a paired device does. Without Connect the daemon names its own
/// listener, and the person sharing knows the guest needs a route to it.
fn share_base(conn: &Connection) -> String {
    if let Some(remote) = &conn.state.remote
        && let Some(client) = remote.client()
        && client.is_logged_in()
        && let Some(base) = client.public_base_url()
        && let Some(machine) = client.machine_id()
    {
        return format!("{}/guest/{machine}", base.trim_end_matches('/'));
    }
    conn.state
        .guest
        .local_base()
        .unwrap_or_else(|| format!("http://127.0.0.1:{}", conn.state.network.snapshot().port))
}

fn internal(error: crate::guest::GuestError) -> ProtoError {
    ProtoError::new(
        h4rn_protocol::ProtoErrorCode::Internal,
        error.to_string(),
        "Fix the daemon data directory, then try again",
    )
}

fn guest_error(error: crate::guest::GuestError) -> ProtoError {
    match error {
        crate::guest::GuestError::NotFound(message) => ProtoError::new(
            h4rn_protocol::ProtoErrorCode::NotFound,
            message,
            "Refresh the thread; the request may have been sent or dismissed already",
        ),
        crate::guest::GuestError::Invalid(message) => ProtoError::new(
            h4rn_protocol::ProtoErrorCode::InvalidRequest,
            message,
            "Check the thread and try again",
        ),
        other => internal(other),
    }
}
