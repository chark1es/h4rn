//! Skill registry, install and sync requests (ADR-034, ADR-035).
//!
//! Every arm lives here so the skills feature can evolve without touching the
//! other services or the core request match.

use std::sync::atomic::{AtomicU64, Ordering};

use h4rn_protocol::{ClientRequest, Event, ProtoError, ResponseBody, ServerFrame};
use tokio::task::JoinHandle;

use crate::server::Connection;
use crate::server::forward::{ForwardConfig, ForwardContext, ForwardFrame, forward_loop};

/// Requests owned by this service.
pub(super) fn matches(request: &ClientRequest) -> bool {
    matches!(
        request,
        ClientRequest::SkillsList { .. }
            | ClientRequest::SkillsAdd { .. }
            | ClientRequest::SkillsUpdate { .. }
            | ClientRequest::SkillsRemove { .. }
            | ClientRequest::SkillsSetEnabled { .. }
            | ClientRequest::SkillsImportPreview { .. }
            | ClientRequest::SkillsImportCommit { .. }
            | ClientRequest::SkillsConflicts { .. }
            | ClientRequest::SkillsResolveConflict { .. }
            | ClientRequest::SkillsSyncNow
            | ClientRequest::SkillsDiff { .. }
    )
}

/// Handles one skills request.
pub(super) async fn handle(
    conn: &Connection,
    request: ClientRequest,
) -> Result<ResponseBody, ProtoError> {
    match request {
        ClientRequest::SkillsList { host } => {
            if let Some(error) = conn.local_host_error(&host) {
                return Err(error);
            }
            conn.state
                .skills
                .list()
                .await
                .map(|inventory| ResponseBody::Skills { inventory })
                .map_err(h4rn_skills::SkillError::into_proto)
        }
        ClientRequest::SkillsAdd {
            host,
            source,
            harnesses,
            name,
        } => {
            if let Some(error) = conn.local_host_error(&host) {
                return Err(error);
            }
            conn.state
                .skills
                .add(&source, &harnesses, name.as_deref())
                .await
                .map(|skill| ResponseBody::SkillAdded { skill })
                .map_err(h4rn_skills::SkillError::into_proto)
        }
        ClientRequest::SkillsUpdate {
            host,
            skill_id,
            source,
        } => {
            if let Some(error) = conn.local_host_error(&host) {
                return Err(error);
            }
            conn.state
                .skills
                .update(&skill_id, &source)
                .await
                .map(|skill| ResponseBody::SkillAdded { skill })
                .map_err(h4rn_skills::SkillError::into_proto)
        }
        ClientRequest::SkillsRemove { host, skill_id } => {
            if let Some(error) = conn.local_host_error(&host) {
                return Err(error);
            }
            conn.state
                .skills
                .remove(&skill_id)
                .await
                .map(|()| ResponseBody::Ack)
                .map_err(h4rn_skills::SkillError::into_proto)
        }
        ClientRequest::SkillsSetEnabled {
            host,
            skill_id,
            harness,
            enabled,
        } => {
            if let Some(error) = conn.local_host_error(&host) {
                return Err(error);
            }
            conn.state
                .skills
                .set_enabled(&skill_id, harness, enabled)
                .await
                .map(|skill| ResponseBody::SkillAdded { skill })
                .map_err(h4rn_skills::SkillError::into_proto)
        }
        ClientRequest::SkillsImportPreview { host } => {
            if let Some(error) = conn.local_host_error(&host) {
                return Err(error);
            }
            conn.state
                .skills
                .import_preview()
                .await
                .map(|preview| ResponseBody::SkillsImportPreview { preview })
                .map_err(h4rn_skills::SkillError::into_proto)
        }
        ClientRequest::SkillsImportCommit {
            host,
            candidate_ids,
        } => {
            if let Some(error) = conn.local_host_error(&host) {
                return Err(error);
            }
            conn.state
                .skills
                .import_commit(&candidate_ids)
                .await
                .map(|_| ResponseBody::Ack)
                .map_err(h4rn_skills::SkillError::into_proto)
        }
        ClientRequest::SkillsConflicts { host } => {
            if let Some(error) = conn.local_host_error(&host) {
                return Err(error);
            }
            conn.state
                .skills
                .conflicts()
                .await
                .map(|conflicts| ResponseBody::SkillConflicts { conflicts })
                .map_err(h4rn_skills::SkillError::into_proto)
        }
        ClientRequest::SkillsResolveConflict {
            host,
            conflict_id,
            resolution,
        } => {
            if let Some(error) = conn.local_host_error(&host) {
                return Err(error);
            }
            conn.state
                .skills
                .resolve_conflict(&conflict_id, resolution)
                .await
                .map(|()| ResponseBody::Ack)
                .map_err(h4rn_skills::SkillError::into_proto)
        }
        ClientRequest::SkillsSyncNow => conn
            .state
            .skills
            .sync_now()
            .await
            .map(|status| ResponseBody::SkillSyncStatus { status })
            .map_err(h4rn_skills::SkillError::into_proto),
        ClientRequest::SkillsDiff {
            host,
            skill_id,
            from_version,
            to_version,
        } => {
            if let Some(error) = conn.local_host_error(&host) {
                return Err(error);
            }
            conn.state
                .skills
                .diff(&skill_id, from_version.as_deref(), to_version.as_deref())
                .await
                .map(|diff| ResponseBody::SkillDiff { diff })
                .map_err(h4rn_skills::SkillError::into_proto)
        }
        other => Err(crate::server::not_implemented(&other)),
    }
}

/// Spawns this service's transient-event forwarder for one connection.
pub(super) fn spawn(conn: &Connection) -> JoinHandle<()> {
    let host = conn.state.skills.host().clone();
    tokio::spawn(forward_loop(
        conn.state.skills.subscribe(),
        ForwardContext::for_connection(conn),
        ForwardConfig {
            label: "skill broadcast",
            topic: Some(h4rn_protocol::Topic::Skills),
            lag: crate::server::forward::LagPolicy::Disconnect,
            full: crate::server::forward::FullPolicy::Disconnect,
        },
        move |notice: h4rn_skills::SkillNotice, last: &AtomicU64| {
            let event = match notice {
                h4rn_skills::SkillNotice::Changed => Event::SkillsChanged {
                    host: host.clone(),
                    at: chrono::Utc::now(),
                },
                h4rn_skills::SkillNotice::Sync(status) => Event::SyncStatus(status),
            };
            Some(ForwardFrame::transient(ServerFrame::Event {
                seq: last.load(Ordering::Relaxed),
                event,
            }))
        },
    ))
}
