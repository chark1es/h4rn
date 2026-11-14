//! CLIProxyAPI profile requests (ADR-015).
//!
//! Every arm lives here so the proxy can evolve without touching the other
//! services or the core request match.

use h4rn_protocol::{ClientRequest, ProtoError, ResponseBody, ServerFrame};
use tokio::task::JoinHandle;

use crate::server::Connection;
use crate::server::forward::{ForwardConfig, ForwardContext, ForwardFrame, forward_loop};

/// Requests owned by this service.
pub(super) fn matches(request: &ClientRequest) -> bool {
    matches!(
        request,
        ClientRequest::ListProxyProfiles
            | ClientRequest::AddProxyProfile { .. }
            | ClientRequest::UpdateProxyProfile { .. }
            | ClientRequest::RemoveProxyProfile { .. }
            | ClientRequest::TestProxyProfile { .. }
            | ClientRequest::ListProxyModels { .. }
            | ClientRequest::SetProxyHarnessOverride { .. }
            | ClientRequest::ApplyProxyOverrides { .. }
            | ClientRequest::ListProxyLimits { .. }
            | ClientRequest::PickProxyFailover { .. }
    )
}

/// Handles one proxy request.
pub(super) async fn handle(
    conn: &Connection,
    request: ClientRequest,
) -> Result<ResponseBody, ProtoError> {
    match request {
        ClientRequest::ListProxyProfiles => Ok(ResponseBody::ProxyProfiles {
            profiles: conn.state.proxy.profiles(),
        }),
        ClientRequest::AddProxyProfile { profile } => conn
            .state
            .proxy
            .add_profile(profile)
            .map(|profile| ResponseBody::ProxyProfile { profile })
            .map_err(h4rn_proxy::ProxyError::into_proto),
        ClientRequest::UpdateProxyProfile { profile_id, patch } => conn
            .state
            .proxy
            .update_profile(&profile_id, patch)
            .map(|profile| ResponseBody::ProxyProfile { profile })
            .map_err(h4rn_proxy::ProxyError::into_proto),
        ClientRequest::RemoveProxyProfile { profile_id } => conn
            .state
            .proxy
            .remove_profile(&profile_id)
            .map(|()| ResponseBody::Ack)
            .map_err(h4rn_proxy::ProxyError::into_proto),
        ClientRequest::TestProxyProfile { profile_id } => conn
            .state
            .proxy
            .test(&profile_id)
            .await
            .map(|report| ResponseBody::ProxyTested { report })
            .map_err(h4rn_proxy::ProxyError::into_proto),
        ClientRequest::ListProxyModels { profile_id } => conn
            .state
            .proxy
            .models(&profile_id)
            .await
            .map(|catalog| ResponseBody::ProxyModels { catalog })
            .map_err(h4rn_proxy::ProxyError::into_proto),
        ClientRequest::SetProxyHarnessOverride {
            profile_id,
            harness,
            enabled,
            default_model,
        } => conn
            .state
            .proxy
            .set_harness_override(&profile_id, harness, enabled, default_model)
            .map(|profile| ResponseBody::ProxyProfile { profile })
            .map_err(h4rn_proxy::ProxyError::into_proto),
        ClientRequest::ApplyProxyOverrides { profile_id } => {
            let profile = conn
                .state
                .proxy
                .profiles()
                .into_iter()
                .find(|profile| profile.id == profile_id)
                .ok_or_else(|| {
                    ProtoError::new(
                        h4rn_protocol::ProtoErrorCode::NotFound,
                        "no such proxy profile",
                        "Reload the profile list.",
                    )
                })?;
            let harnesses: Vec<_> = profile
                .harnesses
                .iter()
                .filter(|routed| routed.enabled)
                .map(|routed| routed.harness)
                .collect();
            let applied = conn.state.manager.restart_sessions(&harnesses).await;
            Ok(ResponseBody::ProxyApplied {
                restarted: applied.restarted,
                interrupted: applied.interrupted,
            })
        }
        ClientRequest::ListProxyLimits { profile_id } => conn
            .state
            .proxy
            .limits(&profile_id)
            .await
            .map(|limits| ResponseBody::ProxyLimits { limits })
            .map_err(h4rn_proxy::ProxyError::into_proto),
        ClientRequest::PickProxyFailover {
            profile_id,
            current_model,
            policy,
        } => conn
            .state
            .proxy
            .pick_failover(&profile_id, &current_model, &policy)
            .await
            .map(|choice| ResponseBody::ProxyFailover { choice })
            .map_err(h4rn_proxy::ProxyError::into_proto),
        other => Err(crate::server::not_implemented(&other)),
    }
}

/// Spawns this service's transient-event forwarder for one connection.
pub(super) fn spawn(conn: &Connection) -> JoinHandle<()> {
    tokio::spawn(forward_loop(
        conn.state.proxy.subscribe(),
        ForwardContext::for_connection(conn),
        ForwardConfig::always_on("proxy event broadcast"),
        |event, _last| Some(ForwardFrame::transient(ServerFrame::ProxyEvent { event })),
    ))
}
