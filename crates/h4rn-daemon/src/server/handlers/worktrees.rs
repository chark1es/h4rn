//! Worktree listing and clearing requests.
//!
//! The tracker lives in [`crate::worktrees`]; this module is the protocol edge.
//! There is no transient event stream: Settings refetches on demand.

use h4rn_protocol::{ClientRequest, ProtoError, ResponseBody};
use tokio::task::JoinHandle;

use crate::server::Connection;

/// Requests owned by this service.
pub(super) fn matches(request: &ClientRequest) -> bool {
    matches!(
        request,
        ClientRequest::ListWorktrees | ClientRequest::ClearWorktrees { .. }
    )
}

/// Handles one worktree request.
pub(super) async fn handle(
    conn: &Connection,
    request: ClientRequest,
) -> Result<ResponseBody, ProtoError> {
    match request {
        ClientRequest::ListWorktrees => Ok(ResponseBody::Worktrees {
            worktrees: conn.state.worktrees.list().await,
        }),
        ClientRequest::ClearWorktrees { paths, force, bulk } => {
            let outcome = conn.state.worktrees.clear(paths, force, bulk).await;
            Ok(ResponseBody::WorktreesCleared {
                removed: outcome.removed,
                skipped: outcome.skipped,
                worktrees: outcome.worktrees,
            })
        }
        other => Err(crate::server::not_implemented(&other)),
    }
}

/// This service has no stream, so its forwarder has nothing to do.
pub(super) fn spawn(_conn: &Connection) -> JoinHandle<()> {
    tokio::spawn(async {})
}
