//! Phone-alert settings from the owner (ADR-048).
//!
//! The switch lives on the daemon because the daemon is what publishes. It is
//! off by default, and turning it off stops the next observe immediately.

use h4rn_protocol::{ClientRequest, ProtoError, ResponseBody};
use tokio::task::JoinHandle;

use crate::server::Connection;

/// Requests owned by this service.
pub(super) fn matches(request: &ClientRequest) -> bool {
    matches!(
        request,
        ClientRequest::GetAgentAlertSettings | ClientRequest::SetAgentAlertSettings { .. }
    )
}

/// Handles one alert-settings request.
pub(super) async fn handle(
    conn: &Connection,
    request: ClientRequest,
) -> Result<ResponseBody, ProtoError> {
    match request {
        ClientRequest::GetAgentAlertSettings => {
            let settings = conn.state.alerts.settings().await;
            Ok(ResponseBody::AgentAlertSettings { settings })
        }
        ClientRequest::SetAgentAlertSettings { enabled } => {
            let settings = conn
                .state
                .alerts
                .set_enabled(enabled)
                .await
                .map_err(|message| {
                    ProtoError::new(
                        h4rn_protocol::ProtoErrorCode::Internal,
                        message,
                        "Fix the daemon data directory, then try again",
                    )
                })?;
            conn.audit(
                "agent_alerts.setting_changed",
                Some("setting"),
                Some("agent_alerts"),
                serde_json::json!({"enabled": enabled}),
            );
            Ok(ResponseBody::AgentAlertSettings { settings })
        }
        other => Err(crate::server::not_implemented(&other)),
    }
}

/// No transient events: alerts are published from the daemon's own stream.
pub(super) fn spawn(_conn: &Connection) -> JoinHandle<()> {
    tokio::spawn(async {})
}
