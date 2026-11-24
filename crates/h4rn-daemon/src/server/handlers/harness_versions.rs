//! Harness CLI version listing and update requests (ADR-043).

use h4rn_protocol::{ClientRequest, ProtoError, ResponseBody, ServerFrame, Topic};
use tokio::task::JoinHandle;

use crate::server::Connection;
use crate::server::forward::{
    ForwardConfig, ForwardContext, ForwardFrame, FullPolicy, LagPolicy, forward_loop,
};

/// Requests owned by this service.
pub(super) fn matches(request: &ClientRequest) -> bool {
    matches!(
        request,
        ClientRequest::ListHarnessVersions
            | ClientRequest::ListHarnessCapabilities
            | ClientRequest::UpdateHarness { .. }
    )
}

/// Handles one harness version request.
pub(super) async fn handle(
    conn: &Connection,
    request: ClientRequest,
) -> Result<ResponseBody, ProtoError> {
    match request {
        ClientRequest::ListHarnessVersions => Ok(ResponseBody::HarnessVersions {
            harnesses: conn.state.harness_versions.list().await,
        }),
        ClientRequest::ListHarnessCapabilities => Ok(ResponseBody::HarnessCapabilities {
            harnesses: [
                h4rn_protocol::HarnessKind::ClaudeCode,
                h4rn_protocol::HarnessKind::Codex,
                h4rn_protocol::HarnessKind::Opencode,
                h4rn_protocol::HarnessKind::Antigravity,
            ]
            .into_iter()
            .map(|harness| h4rn_protocol::HarnessCapabilityEntry {
                harness,
                capabilities: h4rn_harness::capabilities(harness),
            })
            .collect(),
        }),
        ClientRequest::UpdateHarness { harness } => conn
            .state
            .harness_versions
            .start_update(harness)
            .await
            .map(|()| ResponseBody::Ack),
        other => Err(crate::server::not_implemented(&other)),
    }
}

/// Spawns this service's transient-event forwarder for one connection.
pub(super) fn spawn(conn: &Connection) -> JoinHandle<()> {
    tokio::spawn(forward_loop(
        conn.state.harness_versions.subscribe(),
        ForwardContext::for_connection(conn),
        ForwardConfig {
            label: "harness update broadcast",
            topic: Some(Topic::Hosts),
            lag: LagPolicy::Skip,
            full: FullPolicy::Drop,
        },
        |event: h4rn_protocol::HarnessUpdateEvent, _last| {
            Some(ForwardFrame::transient(ServerFrame::HarnessEvent { event }))
        },
    ))
}
