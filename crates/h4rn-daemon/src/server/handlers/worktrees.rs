//! Worktree listing and clearing requests.
//!
//! The tracker lives in [`crate::worktrees`]; this module is the protocol edge.
//! There is no transient event stream: Settings refetches on demand.

use std::path::Path;

use h4rn_protocol::{ClientRequest, ProtoError, ProtoErrorCode, ResponseBody};
use tokio::task::JoinHandle;

use crate::server::Connection;
use crate::worktrees::settings::{expand_location, location_inside_repo, validate_location};

/// Requests owned by this service.
pub(super) fn matches(request: &ClientRequest) -> bool {
    matches!(
        request,
        ClientRequest::ListWorktrees
            | ClientRequest::ClearWorktrees { .. }
            | ClientRequest::GetWorktreeSettings
            | ClientRequest::SetWorktreeSettings { .. }
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
        ClientRequest::GetWorktreeSettings => Ok(ResponseBody::WorktreeSettings {
            settings: conn.state.worktree_settings.get(),
        }),
        ClientRequest::SetWorktreeSettings { settings } => {
            let checked = validate_for_save(conn, &settings).await?;
            conn.state
                .worktree_settings
                .set(checked)
                .map(|settings| ResponseBody::WorktreeSettings { settings })
                .map_err(|message| {
                    let hint = settings_hint(&message);
                    ProtoError::new(ProtoErrorCode::InvalidRequest, message, hint)
                })
        }
        other => Err(crate::server::not_implemented(&other)),
    }
}

/// A location that resolves inside a local project checkout would put new
/// worktrees in that project's main checkout, so it is refused while the person
/// can still fix it instead of failing their first worktree request.
async fn validate_for_save(
    conn: &Connection,
    settings: &h4rn_protocol::WorktreeSettings,
) -> Result<h4rn_protocol::WorktreeSettings, ProtoError> {
    let Some(location) = settings
        .location
        .as_deref()
        .map(str::trim)
        .filter(|location| !location.is_empty())
    else {
        return Ok(settings.clone());
    };
    // An invalid path is refused by `set` with its own message.
    if validate_location(location).is_err() {
        return Ok(settings.clone());
    }
    let Some(expanded) = expand_location(location, dirs::home_dir().as_deref()).ok() else {
        return Ok(settings.clone());
    };
    let local = conn.state.manager.local_host();
    for project in conn.state.projects.list().await {
        if project.host != local || !project.git {
            continue;
        }
        if location_inside_repo(Path::new(&project.path), &expanded) {
            return Err(ProtoError::new(
                ProtoErrorCode::InvalidRequest,
                format!(
                    "the worktree location {} is inside the project checkout {}; new checkouts would sit in the main checkout",
                    expanded.display(),
                    project.path
                ),
                "Choose a folder outside the project checkout",
            ));
        }
    }
    Ok(settings.clone())
}

/// The hint that fits the reason a settings value was refused.
fn settings_hint(message: &str) -> &'static str {
    if message.contains("branch prefix") {
        "Use letters, digits and `-`, `_`, `.` or `/` for the branch prefix, with each `/` part not starting with `.`"
    } else if message.contains("inside the project checkout")
        || message.contains("inside the repository")
    {
        "Choose a folder outside the project checkout"
    } else {
        "Use an absolute path or one starting with `~/`"
    }
}

/// This service has no stream, so its forwarder has nothing to do.
pub(super) fn spawn(_conn: &Connection) -> JoinHandle<()> {
    tokio::spawn(async {})
}
