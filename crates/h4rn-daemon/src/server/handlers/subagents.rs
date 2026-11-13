//! Subagent requests and transient progress (ADR-038).
//!
//! The supervisor lives in [`crate::subagents`]; this module is the protocol
//! edge. Durable spawn/finish rows travel through the parent's journal as
//! ordinary thread events, while progress updates are transient frames gated
//! on [`Topic::Threads`] so a client can ask for the subagent panel's live
//! counters without a second subscription.

use std::sync::atomic::{AtomicU64, Ordering};

use h4rn_protocol::{
    ClientRequest, Event, ProtoError, ResponseBody, ServerFrame, SubagentRequest, Topic,
};
use tokio::task::JoinHandle;

use crate::server::Connection;
use crate::server::forward::{
    ForwardConfig, ForwardContext, ForwardFrame, FullPolicy, LagPolicy, forward_loop,
};
use crate::subagents::{SubagentError, SubagentEvent};

/// Requests owned by this service.
pub(super) fn matches(request: &ClientRequest) -> bool {
    matches!(request, ClientRequest::Subagent { .. })
}

/// Handles one subagent request.
pub(super) async fn handle(
    conn: &Connection,
    request: ClientRequest,
) -> Result<ResponseBody, ProtoError> {
    let ClientRequest::Subagent { request } = request else {
        return Err(crate::server::not_implemented(&request));
    };
    match request {
        SubagentRequest::ListSubagents {
            parent_thread,
            include_finished,
        } => Ok(ResponseBody::Subagents {
            subagents: conn
                .state
                .subagents
                .list(parent_thread.as_ref(), include_finished)
                .await,
        }),
        SubagentRequest::GetSubagent { subagent_id } => conn
            .state
            .subagents
            .get(&subagent_id)
            .await
            .map(|subagent| ResponseBody::Subagent { subagent })
            .ok_or_else(|| {
                SubagentError::NotFound(format!("subagent {subagent_id} does not exist"))
                    .into_proto()
            }),
        SubagentRequest::CancelSubagent { subagent_id } => conn
            .state
            .subagents
            .cancel(&subagent_id)
            .await
            .map(|subagent| ResponseBody::Subagent { subagent })
            .map_err(SubagentError::into_proto),
        SubagentRequest::SpawnSubagent { request } => conn
            .state
            .subagents
            .spawn(request)
            .await
            .map(|subagent| ResponseBody::Subagent { subagent })
            .map_err(SubagentError::into_proto),
    }
}

/// Spawns this service's transient-event forwarder for one connection.
pub(super) fn spawn(conn: &Connection) -> JoinHandle<()> {
    tokio::spawn(forward_loop(
        conn.state.subagents.subscribe(),
        ForwardContext::for_connection(conn),
        ForwardConfig {
            label: "subagent broadcast",
            topic: Some(Topic::Threads),
            lag: LagPolicy::Skip,
            full: FullPolicy::Drop,
        },
        |event: SubagentEvent, last: &AtomicU64| {
            let SubagentEvent::Updated(subagent) = event;
            Some(ForwardFrame::transient(ServerFrame::Event {
                seq: last.load(Ordering::Relaxed),
                event: Event::SubagentUpdated {
                    parent_thread_id: subagent.parent_thread_id.clone(),
                    subagent,
                },
            }))
        },
    ))
}
