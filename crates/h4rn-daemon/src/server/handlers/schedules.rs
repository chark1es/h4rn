//! Scheduled task requests and their transient change stream.
//!
//! The service lives in [`crate::schedules`]; this module is the protocol edge.
//! Lists are `view`; every write is `admin` (see `ClientRequest::required_scope`).

use h4rn_protocol::{ClientRequest, ProtoError, ResponseBody, ServerFrame};
use tokio::task::JoinHandle;

use crate::server::Connection;
use crate::server::forward::{ForwardConfig, ForwardContext, ForwardFrame, forward_loop};

/// Requests owned by this service.
pub(super) fn matches(request: &ClientRequest) -> bool {
    matches!(
        request,
        ClientRequest::ListSchedules
            | ClientRequest::CreateSchedule { .. }
            | ClientRequest::UpdateSchedule { .. }
            | ClientRequest::DeleteSchedule { .. }
            | ClientRequest::SetScheduleEnabled { .. }
            | ClientRequest::RunScheduleNow { .. }
            | ClientRequest::ListScheduleRuns { .. }
            | ClientRequest::RotateScheduleWebhook { .. }
            | ClientRequest::GetScheduleWebhookUrl { .. }
    )
}

/// Handles one scheduled-task request.
pub(super) async fn handle(
    conn: &Connection,
    request: ClientRequest,
) -> Result<ResponseBody, ProtoError> {
    let service = &conn.state.schedules;
    match request {
        ClientRequest::ListSchedules => Ok(ResponseBody::Schedules {
            tasks: service.list().await,
        }),
        ClientRequest::CreateSchedule { task } => service
            .create(task)
            .await
            .map(|task| ResponseBody::ScheduleTask { task })
            .map_err(|error| error.into_proto()),
        ClientRequest::UpdateSchedule { task_id, task } => service
            .update(&task_id, task)
            .await
            .map(|task| ResponseBody::ScheduleTask { task })
            .map_err(|error| error.into_proto()),
        ClientRequest::DeleteSchedule { task_id } => service
            .delete(&task_id)
            .await
            .map(|()| ResponseBody::Ack)
            .map_err(|error| error.into_proto()),
        ClientRequest::SetScheduleEnabled { task_id, enabled } => service
            .set_enabled(&task_id, enabled)
            .await
            .map(|task| ResponseBody::ScheduleTask { task })
            .map_err(|error| error.into_proto()),
        ClientRequest::RunScheduleNow { task_id } => {
            service
                .run_now(&task_id)
                .await
                .map_err(|error| error.into_proto())?;
            match service.task(&task_id).await {
                Some(task) => Ok(ResponseBody::ScheduleTask { task }),
                None => Err(ProtoError::plain(
                    h4rn_protocol::ProtoErrorCode::NotFound,
                    format!("no scheduled task `{task_id}` exists on this daemon"),
                )),
            }
        }
        ClientRequest::ListScheduleRuns { task_id } => service
            .runs(&task_id)
            .await
            .map(|runs| ResponseBody::ScheduleRuns { runs })
            .map_err(|error| error.into_proto()),
        ClientRequest::RotateScheduleWebhook { task_id } => service
            .rotate_webhook(&task_id)
            .await
            .map(|(_, path)| ResponseBody::ScheduleWebhookUrl { task_id, path })
            .map_err(|error| error.into_proto()),
        ClientRequest::GetScheduleWebhookUrl { task_id } => service
            .webhook_url(&task_id)
            .await
            .map(|path| ResponseBody::ScheduleWebhookUrl { task_id, path })
            .map_err(|error| error.into_proto()),
        other => Err(crate::server::not_implemented(&other)),
    }
}

/// Spawns this service's transient-event forwarder for one connection.
pub(super) fn spawn(conn: &Connection) -> JoinHandle<()> {
    tokio::spawn(forward_loop(
        conn.state.schedules.subscribe(),
        ForwardContext::for_connection(conn),
        ForwardConfig::always_on("schedule event broadcast"),
        |event: h4rn_protocol::ScheduleEvent, _last| {
            Some(ForwardFrame::transient(ServerFrame::ScheduleEvent {
                event,
            }))
        },
    ))
}
