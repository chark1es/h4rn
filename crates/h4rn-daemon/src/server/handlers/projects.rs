//! Registered projects: list, add (open, create, clone), rename, remove and
//! directory browsing for the add-project picker.

use h4rn_protocol::{ClientRequest, ProtoError, ResponseBody};
use tokio::task::JoinHandle;

use crate::server::Connection;

/// Requests owned by this service.
pub(super) fn matches(request: &ClientRequest) -> bool {
    matches!(
        request,
        ClientRequest::ListProjects { .. }
            | ClientRequest::AddProject { .. }
            | ClientRequest::RenameProject { .. }
            | ClientRequest::RemoveProject { .. }
            | ClientRequest::BrowseDirectories { .. }
    )
}

/// Handles one project request for this daemon's host.
pub(super) async fn handle(
    conn: &Connection,
    request: ClientRequest,
) -> Result<ResponseBody, ProtoError> {
    let projects = &conn.state.projects;
    match request {
        ClientRequest::ListProjects { host } => {
            if let Some(error) = conn.local_host_error(&host) {
                return Err(error);
            }
            Ok(ResponseBody::Projects {
                projects: projects.list().await,
            })
        }
        ClientRequest::AddProject { host, source, name } => {
            if let Some(error) = conn.local_host_error(&host) {
                return Err(error);
            }
            projects
                .add(source, name)
                .await
                .map(|project| ResponseBody::Project { project })
        }
        ClientRequest::RenameProject {
            host,
            project_id,
            name,
        } => {
            if let Some(error) = conn.local_host_error(&host) {
                return Err(error);
            }
            projects
                .rename(&project_id, &name)
                .await
                .map(|project| ResponseBody::Project { project })
        }
        ClientRequest::RemoveProject { host, project_id } => {
            if let Some(error) = conn.local_host_error(&host) {
                return Err(error);
            }
            projects
                .remove(&project_id)
                .await
                .map(|()| ResponseBody::Ack)
        }
        ClientRequest::BrowseDirectories { host, path } => {
            if let Some(error) = conn.local_host_error(&host) {
                return Err(error);
            }
            projects
                .browse(path.as_deref())
                .map(|listing| ResponseBody::Directories { listing })
        }
        other => Err(crate::server::not_implemented(&other)),
    }
}

/// Projects have no transient event stream yet; clients refetch after a change.
pub(super) fn spawn(_conn: &Connection) -> JoinHandle<()> {
    tokio::spawn(async {})
}
