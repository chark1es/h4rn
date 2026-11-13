//! Durable metering, limits and auto-resume (ADR-024).
//!
//! Every arm lives here so the usage ledger can evolve without touching the
//! other services or the core request match.

use h4rn_protocol::{ClientRequest, ProtoError, ProtoErrorCode, ResponseBody, ServerFrame};
use h4rn_protocol::{Topic, UsageScope};
use tokio::task::JoinHandle;

use crate::server::Connection;
use crate::server::forward::{ForwardConfig, ForwardContext, ForwardFrame, forward_loop};

/// Requests owned by this service.
pub(super) fn matches(request: &ClientRequest) -> bool {
    matches!(
        request,
        ClientRequest::GetUsage { .. }
            | ClientRequest::GetLimits
            | ClientRequest::ListResets
            | ClientRequest::CancelResume { .. }
            | ClientRequest::ResumeNow { .. }
            | ClientRequest::SetAutoResumePolicy { .. }
    )
}

/// Handles one metering request.
pub(super) async fn handle(
    conn: &Connection,
    request: ClientRequest,
) -> Result<ResponseBody, ProtoError> {
    match request {
        ClientRequest::GetUsage {
            scope,
            scope_id,
            range,
            group_by,
            bucket,
            include_children,
        } => {
            if matches!(scope, UsageScope::Thread | UsageScope::Workspace) && scope_id.is_none() {
                Err(ProtoError::new(
                    ProtoErrorCode::InvalidRequest,
                    "a thread or workspace usage query needs a scope_id",
                    "Pass the thread id or workspace path in scope_id",
                ))
            } else {
                conn.state
                    .manager
                    .usage_report(h4rn_meter::ReportQuery {
                        scope,
                        scope_id,
                        range,
                        group_by,
                        bucket,
                        include_children,
                    })
                    .await
                    .map(|report| ResponseBody::UsageReport { report })
                    .map_err(crate::threads::ManagerError::into_proto)
            }
        }
        ClientRequest::GetLimits => conn
            .state
            .manager
            .limits_report()
            .await
            .map(|report| ResponseBody::Limits { report })
            .map_err(crate::threads::ManagerError::into_proto),
        ClientRequest::ListResets => conn
            .state
            .manager
            .resets()
            .await
            .map(|resets| ResponseBody::Resets { resets })
            .map_err(crate::threads::ManagerError::into_proto),
        ClientRequest::CancelResume { thread_id } => conn
            .state
            .manager
            .cancel_resume(&thread_id)
            .await
            .map(|schedule| ResponseBody::ResumeChanged { schedule })
            .map_err(crate::threads::ManagerError::into_proto),
        ClientRequest::ResumeNow { thread_id } => conn
            .state
            .manager
            .resume_now(&thread_id)
            .await
            .map(|schedule| ResponseBody::ResumeChanged { schedule })
            .map_err(crate::threads::ManagerError::into_proto),
        ClientRequest::SetAutoResumePolicy { policy } => {
            conn.state.manager.set_auto_resume_policy(policy);
            Ok(ResponseBody::Ack)
        }
        other => Err(crate::server::not_implemented(&other)),
    }
}

/// Spawns this service's transient-event forwarder for one connection.
pub(super) fn spawn(conn: &Connection) -> JoinHandle<()> {
    tokio::spawn(forward_loop(
        conn.state.meter.subscribe(),
        ForwardContext::for_connection(conn),
        ForwardConfig {
            label: "usage event broadcast",
            topic: Some(Topic::Usage),
            lag: crate::server::forward::LagPolicy::Disconnect,
            full: crate::server::forward::FullPolicy::Disconnect,
        },
        |event: h4rn_protocol::UsageStreamEvent, _last| {
            Some(ForwardFrame::transient(ServerFrame::UsageEvent { event }))
        },
    ))
}
