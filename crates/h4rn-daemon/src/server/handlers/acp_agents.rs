//! User-configured local ACP agent requests.
//!
//! Writes persist to the daemon's data directory and need the `admin` scope;
//! listing is a harmless read. A probe starts the configured command once and
//! reports what it advertises, so it is an admin action too. The handshake can
//! take seconds, so a probe answers on its own task instead of the connection's
//! receive loop; see [`defers_reply`].

use h4rn_protocol::{ClientRequest, ProtoError, ResponseBody};
use tokio::task::JoinHandle;

use crate::server::{AppState, Connection};

/// Requests whose answer takes as long as starting a third-party process.
///
/// Their handler would otherwise hold the connection's receive loop for the
/// whole handshake, so every other request on that socket would wait behind
/// it. The server spawns these and writes the frame when they finish.
pub(in crate::server) fn defers_reply(request: &ClientRequest) -> bool {
    matches!(request, ClientRequest::ProbeAcpAgent { .. })
}

/// Answers one deferred request, off the connection's receive loop.
pub(in crate::server) async fn handle_deferred(
    state: &AppState,
    request: ClientRequest,
) -> Result<ResponseBody, ProtoError> {
    match request {
        ClientRequest::ProbeAcpAgent { id } => state
            .acp_agents
            .probe(&id)
            .await
            .map(|probe| ResponseBody::AcpAgentProbe { probe }),
        other => Err(crate::server::not_implemented(&other)),
    }
}

/// Requests owned by this service.
pub(super) fn matches(request: &ClientRequest) -> bool {
    matches!(
        request,
        ClientRequest::ListAcpAgents
            | ClientRequest::GetAcpAgent { .. }
            | ClientRequest::AddAcpAgent { .. }
            | ClientRequest::UpdateAcpAgent { .. }
            | ClientRequest::RemoveAcpAgent { .. }
            | ClientRequest::ProbeAcpAgent { .. }
    )
}

/// Handles one ACP agent request.
pub(super) async fn handle(
    conn: &Connection,
    request: ClientRequest,
) -> Result<ResponseBody, ProtoError> {
    match request {
        ClientRequest::ListAcpAgents => Ok(ResponseBody::AcpAgents {
            agents: conn.state.acp_agents.list(),
        }),
        ClientRequest::GetAcpAgent { id } => conn
            .state
            .acp_agents
            .spec(&id)
            .map(|agent| ResponseBody::AcpAgentSpec { agent })
            .ok_or_else(|| {
                ProtoError::plain(
                    h4rn_protocol::ProtoErrorCode::NotFound,
                    format!("no ACP agent `{id}` is configured on this host"),
                )
            }),
        ClientRequest::AddAcpAgent { agent } => conn
            .state
            .acp_agents
            .add(agent)
            .map(|agent| ResponseBody::AcpAgent { agent }),
        ClientRequest::UpdateAcpAgent { id, agent } => conn
            .state
            .acp_agents
            .update(&id, agent)
            .map(|agent| ResponseBody::AcpAgent { agent }),
        ClientRequest::RemoveAcpAgent { id } => conn
            .state
            .acp_agents
            .remove(&id)
            .map(|()| ResponseBody::Ack),
        ClientRequest::ProbeAcpAgent { id } => conn
            .state
            .acp_agents
            .probe(&id)
            .await
            .map(|probe| ResponseBody::AcpAgentProbe { probe }),
        other => Err(crate::server::not_implemented(&other)),
    }
}

/// This service has no event stream of its own.
pub(super) fn spawn(_conn: &Connection) -> JoinHandle<()> {
    tokio::spawn(async {})
}
