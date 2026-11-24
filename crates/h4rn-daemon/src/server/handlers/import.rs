//! Importing conversations that Claude Code and Codex already saved.
//!
//! Scanning is read-only and needs `view`; starting an import creates projects
//! and threads, so it needs `approve`. Progress is a transient frame; a client
//! that reconnects asks for the current run with `SessionImportStatus`.

use h4rn_protocol::{ClientRequest, ProtoError, ResponseBody, ServerFrame};
use tokio::task::JoinHandle;

use crate::server::Connection;
use crate::server::forward::{ForwardConfig, ForwardContext, ForwardFrame, forward_loop};

/// Requests owned by this service.
pub(super) fn matches(request: &ClientRequest) -> bool {
    matches!(
        request,
        ClientRequest::ScanSessionImports { .. }
            | ClientRequest::ImportSessions { .. }
            | ClientRequest::SessionImportStatus { .. }
    )
}

/// Handles one import request for this daemon's host.
pub(super) async fn handle(
    conn: &Connection,
    request: ClientRequest,
) -> Result<ResponseBody, ProtoError> {
    let import = &conn.state.import;
    match request {
        ClientRequest::ScanSessionImports { host } => {
            if let Some(error) = conn.local_host_error(&host) {
                return Err(error);
            }
            Ok(ResponseBody::SessionImports {
                scan: import.scan().await,
            })
        }
        ClientRequest::ImportSessions { host, projects } => {
            if let Some(error) = conn.local_host_error(&host) {
                return Err(error);
            }
            import
                .start(projects)
                .await
                .map(|progress| ResponseBody::SessionImportStatus {
                    progress: Some(progress),
                })
        }
        ClientRequest::SessionImportStatus { host } => {
            if let Some(error) = conn.local_host_error(&host) {
                return Err(error);
            }
            Ok(ResponseBody::SessionImportStatus {
                progress: import.status().await,
            })
        }
        other => Err(crate::server::not_implemented(&other)),
    }
}

/// Spawns this service's progress forwarder for one connection.
pub(super) fn spawn(conn: &Connection) -> JoinHandle<()> {
    tokio::spawn(forward_loop(
        conn.state.import.subscribe(),
        ForwardContext::for_connection(conn),
        ForwardConfig::always_on("import progress broadcast"),
        |event: h4rn_protocol::ImportEvent, _last| {
            Some(ForwardFrame::transient(ServerFrame::ImportEvent { event }))
        },
    ))
}
