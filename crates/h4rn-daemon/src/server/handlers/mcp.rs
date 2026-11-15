//! MCP hub requests and transient events (ADR-006, ADR-036, ADR-037).
//!
//! The hub itself lives in [`crate::mcp`]; this module is the protocol edge:
//! one request envelope plus the legacy variants older clients still send, and
//! the forwarder that pushes status and tool-call events at subscribers of
//! [`h4rn_protocol::Topic::Mcp`].

use std::sync::atomic::{AtomicU64, Ordering};

use h4rn_mcp::hub::HubEvent;
use h4rn_protocol::{
    ClientRequest, Event, McpRequest, ProtoError, ResponseBody, ServerFrame, Topic,
};
use tokio::task::JoinHandle;

use crate::server::Connection;
use crate::server::forward::{
    ForwardConfig, ForwardContext, ForwardFrame, FullPolicy, LagPolicy, forward_loop,
};

/// Requests owned by this service.
pub(super) fn matches(request: &ClientRequest) -> bool {
    matches!(
        request,
        ClientRequest::Mcp { .. }
            | ClientRequest::McpList
            | ClientRequest::McpUpsert { .. }
            | ClientRequest::McpRemove { .. }
    )
}

/// Handles one MCP request.
pub(super) async fn handle(
    conn: &Connection,
    request: ClientRequest,
) -> Result<ResponseBody, ProtoError> {
    match request {
        ClientRequest::Mcp { request } => conn.state.mcp.handle(request).await,
        ClientRequest::McpList => conn.state.mcp.handle(McpRequest::ListServers).await,
        ClientRequest::McpUpsert { input } => {
            conn.state.mcp.handle(McpRequest::Upsert { input }).await
        }
        ClientRequest::McpRemove { name } => {
            conn.state.mcp.handle(McpRequest::Remove { name }).await
        }
        other => Err(crate::server::not_implemented(&other)),
    }
}

/// Spawns this service's transient-event forwarder for one connection.
///
/// Hub events carry no durable sequence (a server can start or fail with no
/// turn in flight), so each frame echoes the cursor the connection has already
/// reached, exactly like `FsChanged`. A lagged client skips missed events
/// rather than reconnecting: status is re-listed on demand.
pub(super) fn spawn(conn: &Connection) -> JoinHandle<()> {
    tokio::spawn(forward_loop(
        conn.state.mcp.hub().subscribe(),
        ForwardContext::for_connection(conn),
        ForwardConfig {
            label: "MCP hub broadcast",
            topic: Some(Topic::Mcp),
            lag: LagPolicy::Skip,
            full: FullPolicy::Drop,
        },
        |event: HubEvent, last: &AtomicU64| map_event(event, last),
    ))
}

/// Maps one hub event onto a transient frame, dropping internal cache changes.
fn map_event(event: HubEvent, last: &AtomicU64) -> Option<ForwardFrame> {
    let event = match event {
        HubEvent::Status {
            server,
            status,
            tool_count,
            message,
            at,
        } => Event::McpServerStatus {
            server,
            status,
            tool_count,
            message,
            at,
        },
        HubEvent::ToolCalled {
            server,
            tool,
            qualified_name,
            caller,
            ok,
            duration_ms,
            error,
            at,
        } => Event::McpToolCalled {
            server,
            tool,
            qualified_name,
            caller,
            ok,
            duration_ms,
            error,
            at,
        },
        HubEvent::ShareApprovalRequested { approval } => {
            Event::McpShareApprovalRequested { approval }
        }
        HubEvent::ShareApprovalResolved {
            approval_id,
            decision,
        } => Event::McpShareApprovalResolved {
            approval_id,
            decision,
        },
        // Cache invalidation is internal; the UI learns the new list when it
        // asks for it.
        HubEvent::ToolsChanged { .. } => return None,
    };
    Some(ForwardFrame::transient(ServerFrame::Event {
        seq: last.load(Ordering::Relaxed),
        event,
    }))
}
