//! Text generation: thread titles, generators and their settings.
//!
//! Generator keys travel only into the daemon; every response reports whether
//! a key exists. Writes and tests need the `admin` scope, listing is `view`.
//! Renaming a thread is the owner's action and needs `approve`.

use h4rn_protocol::{ClientRequest, ProtoError, ResponseBody};
use tokio::task::JoinHandle;

use crate::server::Connection;

/// Requests owned by this service.
pub(super) fn matches(request: &ClientRequest) -> bool {
    matches!(
        request,
        ClientRequest::ListTextGenerators
            | ClientRequest::AddTextGenerator { .. }
            | ClientRequest::UpdateTextGenerator { .. }
            | ClientRequest::RemoveTextGenerator { .. }
            | ClientRequest::TestTextGenerator { .. }
            | ClientRequest::GetTextGenerationSettings
            | ClientRequest::SetTextGenerationSettings { .. }
            | ClientRequest::RenameThread { .. }
    )
}

/// Handles one text-generation or rename request.
pub(super) async fn handle(
    conn: &Connection,
    request: ClientRequest,
) -> Result<ResponseBody, ProtoError> {
    match request {
        ClientRequest::ListTextGenerators => Ok(ResponseBody::TextGenerators {
            generators: conn.state.text_generation.generators(),
        }),
        ClientRequest::AddTextGenerator { generator } => {
            let generator = conn.state.text_generation.add_generator(generator)?;
            conn.audit(
                "text_generator.added",
                Some("text_generator"),
                Some(generator.id.as_str()),
                serde_json::json!({
                    "name": generator.name,
                    "kind": generator.kind,
                    "base_url": generator.base_url,
                }),
            );
            Ok(ResponseBody::TextGenerator { generator })
        }
        ClientRequest::UpdateTextGenerator {
            generator_id,
            patch,
        } => {
            let generator = conn
                .state
                .text_generation
                .update_generator(&generator_id, patch)?;
            conn.audit(
                "text_generator.updated",
                Some("text_generator"),
                Some(generator.id.as_str()),
                serde_json::json!({
                    "name": generator.name,
                    "kind": generator.kind,
                    "base_url": generator.base_url,
                    "has_api_key": generator.has_api_key,
                }),
            );
            Ok(ResponseBody::TextGenerator { generator })
        }
        ClientRequest::RemoveTextGenerator { generator_id } => {
            conn.state.text_generation.remove_generator(&generator_id)?;
            conn.audit(
                "text_generator.removed",
                Some("text_generator"),
                Some(generator_id.as_str()),
                serde_json::json!({}),
            );
            Ok(ResponseBody::Ack)
        }
        ClientRequest::TestTextGenerator {
            generator_id,
            model,
        } => {
            let report = conn
                .state
                .text_generation
                .test_generator(&generator_id, model.as_deref())
                .await?;
            conn.audit(
                "text_generator.tested",
                Some("text_generator"),
                Some(generator_id.as_str()),
                serde_json::json!({
                    "ok": report.ok,
                    "latency_ms": report.latency_ms,
                }),
            );
            Ok(ResponseBody::TextGeneratorTested { report })
        }
        ClientRequest::GetTextGenerationSettings => Ok(ResponseBody::TextGenerationSettings {
            settings: conn.state.text_generation.settings(),
        }),
        ClientRequest::SetTextGenerationSettings { settings } => {
            let settings = conn.state.text_generation.set_settings(settings)?;
            conn.audit(
                "text_generation.settings_changed",
                Some("setting"),
                Some("text_generation"),
                serde_json::json!({ "title": settings.title }),
            );
            Ok(ResponseBody::TextGenerationSettings { settings })
        }
        ClientRequest::RenameThread { thread_id, title } => {
            let summary = conn
                .state
                .manager
                .rename_thread(&thread_id, &title)
                .await
                .map_err(crate::threads::ManagerError::into_proto)?;
            conn.audit(
                "thread.renamed",
                Some("thread"),
                Some(thread_id.as_str()),
                serde_json::json!({ "title": summary.title }),
            );
            Ok(ResponseBody::ThreadRenamed { summary })
        }
        other => Err(crate::server::not_implemented(&other)),
    }
}

/// Settings live in files and renames arrive as normal thread upserts, so the
/// service has no transient event stream of its own.
pub(super) fn spawn(_conn: &Connection) -> JoinHandle<()> {
    tokio::spawn(async {})
}
