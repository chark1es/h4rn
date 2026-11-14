//! Forge requests and watched-PR checks forwarding.
//!
//! Every arm lives here so the forge layer can evolve without touching the
//! other services or the core request match.

use std::sync::atomic::Ordering;

use h4rn_protocol::{ClientRequest, Event, ProtoError, ResponseBody, ServerFrame};
use serde_json::json;
use tokio::task::JoinHandle;

use crate::forge::{ForgeChecksUpdate, ForgeServiceError};
use crate::server::Connection;
use crate::server::forward::{
    ForwardConfig, ForwardContext, ForwardFrame, FullPolicy, LagPolicy, forward_loop,
};

/// Requests owned by this service.
pub(super) fn matches(request: &ClientRequest) -> bool {
    matches!(
        request,
        ClientRequest::ForgeListAccounts
            | ClientRequest::ForgeAddAccount { .. }
            | ClientRequest::ForgeDeviceFlowStart { .. }
            | ClientRequest::ForgeDeviceFlowPoll { .. }
            | ClientRequest::ForgeImportCli { .. }
            | ClientRequest::ForgeRemoveAccount { .. }
            | ClientRequest::ForgeDetectRepo { .. }
            | ClientRequest::ForgeListPrs { .. }
            | ClientRequest::ForgeGetPr { .. }
            | ClientRequest::ForgeGetPrDiff { .. }
            | ClientRequest::ForgeListPrComments { .. }
            | ClientRequest::ForgeListChecks { .. }
            | ClientRequest::ForgeListPrCommits { .. }
            | ClientRequest::ForgeRerunChecks { .. }
            | ClientRequest::ForgeReply { .. }
            | ClientRequest::ForgeComment { .. }
            | ClientRequest::ForgeOpenPr { .. }
            | ClientRequest::ForgeHandPrToAgent { .. }
    )
}

/// Handles one forge request.
pub(super) async fn handle(
    conn: &Connection,
    request: ClientRequest,
) -> Result<ResponseBody, ProtoError> {
    match request {
        ClientRequest::ForgeListAccounts => {
            let (accounts, capabilities) = conn.state.forge.list_accounts();
            Ok(ResponseBody::ForgeAccounts {
                accounts,
                capabilities,
            })
        }
        ClientRequest::ForgeAddAccount {
            forge,
            base_url,
            token,
            username,
            source,
        } => conn
            .state
            .forge
            .add_account(forge, &base_url, &token, username, source)
            .await
            .map(|account| ResponseBody::ForgeAccount { account })
            .map_err(ForgeServiceError::into_proto),
        ClientRequest::ForgeDeviceFlowStart { base_url } => conn
            .state
            .forge
            .start_device_flow(base_url)
            .await
            .map(|flow| ResponseBody::ForgeDeviceFlow { flow })
            .map_err(ForgeServiceError::into_proto),
        ClientRequest::ForgeDeviceFlowPoll { flow_id } => conn
            .state
            .forge
            .poll_device_flow(&flow_id)
            .await
            .map(|status| ResponseBody::ForgeDeviceFlowStatus { status })
            .map_err(ForgeServiceError::into_proto),
        ClientRequest::ForgeImportCli { forge, base_url } => conn
            .state
            .forge
            .import_cli_account(forge, base_url)
            .await
            .map(|account| ResponseBody::ForgeAccount { account })
            .map_err(ForgeServiceError::into_proto),
        ClientRequest::ForgeRemoveAccount { account_id } => conn
            .state
            .forge
            .remove_account(&account_id)
            .map(|()| ResponseBody::Ack)
            .map_err(ForgeServiceError::into_proto),
        ClientRequest::ForgeDetectRepo { workspace } => conn
            .state
            .forge
            .detect_repo(&workspace)
            .map(|repos| ResponseBody::ForgeRepos { repos })
            .map_err(ForgeServiceError::into_proto),
        ClientRequest::ForgeListPrs {
            account_id,
            repo,
            filter,
            page,
        } => conn
            .state
            .forge
            .list_prs(account_id.as_deref(), &repo, &filter, page)
            .await
            .map(|result| ResponseBody::ForgePrs {
                prs: result.prs,
                page: result.page,
                has_more: result.has_more,
            })
            .map_err(ForgeServiceError::into_proto),
        ClientRequest::ForgeGetPr {
            account_id,
            repo,
            number,
        } => conn
            .state
            .forge
            .get_pr(account_id.as_deref(), &repo, number)
            .await
            .map(|pr| ResponseBody::ForgePr { pr })
            .map_err(ForgeServiceError::into_proto),
        ClientRequest::ForgeGetPrDiff {
            account_id,
            repo,
            number,
            page,
            per_page,
        } => conn
            .state
            .forge
            .get_diff(account_id.as_deref(), &repo, number, page, per_page)
            .await
            .map(|diff| ResponseBody::ForgeDiff { diff })
            .map_err(ForgeServiceError::into_proto),
        ClientRequest::ForgeListPrComments {
            account_id,
            repo,
            number,
        } => conn
            .state
            .forge
            .list_comments(account_id.as_deref(), &repo, number)
            .await
            .map(|page| ResponseBody::ForgeComments { page })
            .map_err(ForgeServiceError::into_proto),
        ClientRequest::ForgeListChecks {
            account_id,
            repo,
            number,
        } => conn
            .state
            .forge
            .list_checks(account_id.as_deref(), &repo, number)
            .await
            .map(|checks| ResponseBody::ForgeChecks { checks })
            .map_err(ForgeServiceError::into_proto),
        ClientRequest::ForgeListPrCommits {
            account_id,
            repo,
            number,
        } => conn
            .state
            .forge
            .list_commits(account_id.as_deref(), &repo, number)
            .await
            .map(|commits| ResponseBody::ForgeCommits { commits })
            .map_err(ForgeServiceError::into_proto),
        ClientRequest::ForgeRerunChecks {
            account_id,
            repo,
            number,
        } => conn
            .state
            .forge
            .rerun_checks(account_id.as_deref(), &repo, number)
            .await
            .map(|checks| ResponseBody::ForgeChecks { checks })
            .map_err(ForgeServiceError::into_proto),
        ClientRequest::ForgeReply {
            account_id,
            repo,
            number,
            thread_id,
            comment_id,
            body,
        } => {
            let result = conn
                .state
                .forge
                .reply(
                    account_id.as_deref(),
                    &repo,
                    number,
                    thread_id.as_deref(),
                    &comment_id,
                    &body,
                )
                .await;
            if result.is_ok() {
                conn.audit(
                    "forge.reply",
                    Some("forge_pr"),
                    Some(&format!("{}#{number}", repo.full_name())),
                    json!({"thread": thread_id, "comment": comment_id, "chars": body.len()}),
                );
            }
            result
                .map(|comment| ResponseBody::ForgeReviewCommentPosted { comment })
                .map_err(ForgeServiceError::into_proto)
        }
        ClientRequest::ForgeComment {
            account_id,
            repo,
            number,
            body,
        } => {
            let result = conn
                .state
                .forge
                .create_comment(account_id.as_deref(), &repo, number, &body)
                .await;
            if result.is_ok() {
                conn.audit(
                    "forge.comment",
                    Some("forge_pr"),
                    Some(&format!("{}#{number}", repo.full_name())),
                    json!({"chars": body.len()}),
                );
            }
            result
                .map(|comment| ResponseBody::ForgeCommentPosted { comment })
                .map_err(ForgeServiceError::into_proto)
        }
        ClientRequest::ForgeOpenPr {
            thread_id,
            repo,
            title,
            body,
            base,
        } => {
            let result = conn
                .state
                .forge
                .open_pr(&conn.state.manager, &thread_id, repo, &title, &body, &base)
                .await;
            if let Ok(pr) = &result {
                conn.audit(
                    "forge.open_pr",
                    Some("forge_pr"),
                    Some(&format!(
                        "{}#{}",
                        pr.summary.repo.full_name(),
                        pr.summary.number
                    )),
                    json!({"thread": thread_id, "base": base}),
                );
            }
            result
                .map(|pr| ResponseBody::ForgePrOpened { pr })
                .map_err(ForgeServiceError::into_proto)
        }
        ClientRequest::ForgeHandPrToAgent {
            account_id,
            repo,
            number,
            thread_id,
            target,
        } => {
            if let Some(target) = &target
                && let Some(error) = conn.local_host_error(&target.host)
            {
                Err(error)
            } else {
                conn.state
                    .forge
                    .hand_pr_to_agent(
                        &conn.state.manager,
                        account_id.as_deref(),
                        &repo,
                        number,
                        thread_id.as_ref(),
                        target,
                    )
                    .await
                    .map(|handoff| ResponseBody::ForgeHandoff { handoff })
                    .map_err(ForgeServiceError::into_proto)
            }
        }
        other => Err(crate::server::not_implemented(&other)),
    }
}

/// Spawns this service's transient-event forwarder for one connection.
pub(super) fn spawn(conn: &Connection) -> JoinHandle<()> {
    tokio::spawn(forward_loop(
        conn.state.forge.subscribe(),
        ForwardContext::for_connection(conn),
        ForwardConfig {
            label: "forge checks broadcast",
            topic: None,
            lag: LagPolicy::Skip,
            full: FullPolicy::Drop,
        },
        |update: ForgeChecksUpdate, last| {
            Some(ForwardFrame::transient(ServerFrame::Event {
                // Checks updates are transient: they carry the durable sequence
                // this connection has already reached, not a new journal row.
                seq: last.load(Ordering::Relaxed),
                event: Event::ForgeChecksUpdated {
                    repo: update.repo,
                    number: update.number,
                    checks: update.checks,
                },
            }))
        },
    ))
}
