//! Git actions for a thread: status, commit, push and generated text.
//!
//! Status is a quick read. Commit, push and generation can take as long as a
//! hook, a remote or a model does, so the connection defers them to their own
//! task (see [`defers_reply`]) and keeps answering other frames meanwhile.

use h4rn_protocol::{ClientRequest, ProtoError, ResponseBody};
use tokio::task::JoinHandle;

use crate::git_actions;
use crate::server::{AppState, Connection, SessionOrigin};
use crate::text_generation::TextGeneration as _;

/// Requests owned by this module.
pub(super) fn matches(request: &ClientRequest) -> bool {
    matches!(
        request,
        ClientRequest::GitStatus { .. }
            | ClientRequest::GitCommit { .. }
            | ClientRequest::GitPush { .. }
            | ClientRequest::GenerateGitCommitMessage { .. }
            | ClientRequest::GeneratePullRequestText { .. }
    )
}

/// Whether the request should answer from its own task.
pub(crate) fn defers_reply(request: &ClientRequest) -> bool {
    matches!(
        request,
        ClientRequest::GitCommit { .. }
            | ClientRequest::GitPush { .. }
            | ClientRequest::GenerateGitCommitMessage { .. }
            | ClientRequest::GeneratePullRequestText { .. }
    )
}

/// No transient event stream: the forwarder finishes at once.
pub(super) fn spawn(_conn: &Connection) -> JoinHandle<()> {
    tokio::spawn(async {})
}

/// Handles one git request on the connection's own loop.
pub(super) async fn handle(
    conn: &Connection,
    request: ClientRequest,
) -> Result<ResponseBody, ProtoError> {
    match request {
        ClientRequest::GitStatus { thread_id } => {
            let status = git_actions::status(&conn.state.manager, &thread_id).await?;
            Ok(ResponseBody::GitStatus { status })
        }
        // The read loop defers these; handling one here is the same work.
        other => run(&conn.state, &conn.origin, other).await,
    }
}

/// Handles one deferred git request.
pub(crate) async fn handle_deferred(
    state: &AppState,
    origin: &SessionOrigin,
    request: ClientRequest,
) -> Result<ResponseBody, ProtoError> {
    run(state, origin, request).await
}

/// Runs one git action and records what a remote session asked for.
async fn run(
    state: &AppState,
    origin: &SessionOrigin,
    request: ClientRequest,
) -> Result<ResponseBody, ProtoError> {
    match request {
        ClientRequest::GitStatus { thread_id } => {
            let status = git_actions::status(&state.manager, &thread_id).await?;
            Ok(ResponseBody::GitStatus { status })
        }
        ClientRequest::GitCommit {
            thread_id,
            message,
            include_unstaged,
            paths,
        } => {
            let result =
                git_actions::commit(&state.manager, &thread_id, message, include_unstaged, paths)
                    .await?;
            crate::server::record_audit(
                state,
                origin,
                "git.committed",
                Some("thread"),
                Some(thread_id.as_str()),
                serde_json::json!({
                    "commit": result.commit,
                    "include_unstaged": include_unstaged,
                }),
            );
            Ok(ResponseBody::GitCommitted { result })
        }
        ClientRequest::GitPush {
            thread_id,
            set_upstream,
        } => {
            let result = git_actions::push(&state.manager, &thread_id, set_upstream).await?;
            crate::server::record_audit(
                state,
                origin,
                "git.pushed",
                Some("thread"),
                Some(thread_id.as_str()),
                serde_json::json!({
                    "branch": result.branch,
                    "remote_branch": result.remote_branch,
                    "set_upstream": set_upstream,
                }),
            );
            Ok(ResponseBody::GitPushed { result })
        }
        ClientRequest::GenerateGitCommitMessage {
            thread_id,
            include_unstaged,
            paths,
        } => {
            if !state.text_generation.source_control_enabled().await {
                return Ok(ResponseBody::GeneratedCommitMessage {
                    message: String::new(),
                });
            }
            let harness = thread_harness(state, &thread_id).await?;
            let input =
                git_actions::commit_input(&state.manager, &thread_id, include_unstaged, paths)
                    .await?;
            let message = state
                .text_generation
                .commit_message(harness, &input.diff, &input.recent_subjects)
                .await?;
            crate::server::record_audit(
                state,
                origin,
                "git_text.generated",
                Some("thread"),
                Some(thread_id.as_str()),
                serde_json::json!({
                    "kind": "commit_message",
                    "chars": message.len(),
                }),
            );
            Ok(ResponseBody::GeneratedCommitMessage { message })
        }
        ClientRequest::GeneratePullRequestText { thread_id, base } => {
            if !state.text_generation.source_control_enabled().await {
                return Ok(ResponseBody::GeneratedPullRequestText {
                    title: String::new(),
                    body: String::new(),
                });
            }
            let harness = thread_harness(state, &thread_id).await?;
            let input = git_actions::pull_request_input(&state.manager, &thread_id, base).await?;
            let text = state
                .text_generation
                .pull_request(harness, &input.commits, &input.diff_stat)
                .await?;
            crate::server::record_audit(
                state,
                origin,
                "git_text.generated",
                Some("thread"),
                Some(thread_id.as_str()),
                serde_json::json!({
                    "kind": "pull_request",
                    "base": input.base,
                    "chars": text.body.len(),
                }),
            );
            Ok(ResponseBody::GeneratedPullRequestText {
                title: text.title,
                body: text.body,
            })
        }
        other => Err(ProtoError::new(
            h4rn_protocol::ProtoErrorCode::NotImplemented,
            format!("{other:?} is not a git action"),
            "Reload the client; this daemon does not serve that request here.",
        )),
    }
}

/// The thread's own harness, so the default backend follows the thread.
async fn thread_harness(
    state: &AppState,
    thread_id: &h4rn_protocol::ThreadId,
) -> Result<h4rn_protocol::HarnessKind, ProtoError> {
    let snapshot = state
        .manager
        .get_thread(thread_id, 0)
        .await
        .map_err(|error| error.into_proto())?;
    Ok(snapshot.summary.harness)
}
