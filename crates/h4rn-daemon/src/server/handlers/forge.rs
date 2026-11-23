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
            | ClientRequest::LinkPullRequest { .. }
            | ClientRequest::UnlinkPullRequest { .. }
            | ClientRequest::ListThreadPullRequests { .. }
            | ClientRequest::ListPrThreads { .. }
            | ClientRequest::WatchPullRequest { .. }
            | ClientRequest::UnwatchPullRequest { .. }
            | ClientRequest::ForgeListIssues { .. }
            | ClientRequest::ForgeGetIssue { .. }
            | ClientRequest::ForgeListIssueComments { .. }
            | ClientRequest::ForgeCommentIssue { .. }
            | ClientRequest::ForgeUpdateIssue { .. }
            | ClientRequest::LinearListAccounts
            | ClientRequest::LinearConnect { .. }
            | ClientRequest::LinearRemoveAccount { .. }
            | ClientRequest::LinearListTeams { .. }
            | ClientRequest::LinearProjectTeams
            | ClientRequest::LinearSetProjectTeam { .. }
            | ClientRequest::LinearListIssues { .. }
            | ClientRequest::LinearGetIssue { .. }
            | ClientRequest::LinearCommentIssue { .. }
            | ClientRequest::LinkIssue { .. }
            | ClientRequest::UnlinkIssue { .. }
            | ClientRequest::ListThreadIssues { .. }
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
                linear: Some(crate::linear::capabilities()),
            })
        }
        ClientRequest::ForgeAddAccount {
            forge,
            base_url,
            token,
            username,
            source,
            repository,
        } => conn
            .state
            .forge
            .add_account(forge, &base_url, &token, username, source, repository)
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
            draft,
        } => {
            let result = conn
                .state
                .forge
                .open_pr(
                    &conn.state.manager,
                    &thread_id,
                    repo,
                    &title,
                    &body,
                    &base,
                    draft,
                )
                .await;
            if let Ok(pr) = &result {
                if let Err(error) = conn
                    .state
                    .pr_links
                    .link_opened(&thread_id, &pr.summary.repo, pr.summary.number)
                    .await
                {
                    tracing::warn!(%error, "could not link the PR just opened");
                }
                conn.audit(
                    "forge.open_pr",
                    Some("forge_pr"),
                    Some(&format!(
                        "{}#{}",
                        pr.summary.repo.full_name(),
                        pr.summary.number
                    )),
                    json!({"thread": thread_id, "base": base, "draft": draft}),
                );
            }
            result
                .map(|pr| ResponseBody::ForgePrOpened { pr })
                .map_err(ForgeServiceError::into_proto)
        }
        ClientRequest::LinkPullRequest {
            thread_id,
            account_id,
            repo,
            number,
        } => conn
            .state
            .pr_links
            .link(&thread_id, account_id.as_deref(), &repo, number)
            .await
            .map(|pull| ResponseBody::ThreadPullRequest { pull })
            .map_err(crate::pr_links::PrLinkError::into_proto),
        ClientRequest::UnlinkPullRequest {
            thread_id,
            repo,
            number,
        } => conn
            .state
            .pr_links
            .unlink(&thread_id, &repo, number)
            .await
            .map(|()| ResponseBody::Ack)
            .map_err(crate::pr_links::PrLinkError::into_proto),
        ClientRequest::ListThreadPullRequests { thread_id } => conn
            .state
            .pr_links
            .list(&thread_id)
            .await
            .map(|pulls| ResponseBody::ThreadPullRequests { thread_id, pulls })
            .map_err(crate::pr_links::PrLinkError::into_proto),
        ClientRequest::ListPrThreads { repo, number } => conn
            .state
            .pr_links
            .list_for_pr(&repo, number)
            .await
            .map(|threads| ResponseBody::PrThreads {
                repo,
                number,
                threads,
            })
            .map_err(crate::pr_links::PrLinkError::into_proto),
        ClientRequest::WatchPullRequest {
            thread_id,
            repo,
            number,
        } => conn
            .state
            .pr_links
            .watch(&thread_id, &repo, number)
            .await
            .map(|()| ResponseBody::Ack)
            .map_err(crate::pr_links::PrLinkError::into_proto),
        ClientRequest::UnwatchPullRequest {
            thread_id,
            repo,
            number,
        } => {
            conn.state.pr_links.unwatch(&thread_id, &repo, number);
            Ok(ResponseBody::Ack)
        }
        ClientRequest::ForgeListIssues {
            account_id,
            repo,
            filter,
            page,
            query,
        } => {
            let read_only = !repo_is_owned(conn, &repo).await;
            conn.state
                .forge
                .list_issues(account_id.as_deref(), &repo, filter, page, query.as_deref())
                .await
                .map(|result| ResponseBody::ForgeIssues {
                    issues: result.issues,
                    page: result.page,
                    has_more: result.has_more,
                    read_only,
                })
                .map_err(ForgeServiceError::into_proto)
        }
        ClientRequest::ForgeGetIssue {
            account_id,
            repo,
            number,
        } => {
            let read_only = !repo_is_owned(conn, &repo).await;
            conn.state
                .forge
                .get_issue(account_id.as_deref(), &repo, number)
                .await
                .map(|issue| ResponseBody::ForgeIssue { issue, read_only })
                .map_err(ForgeServiceError::into_proto)
        }
        ClientRequest::ForgeListIssueComments {
            account_id,
            repo,
            number,
            cursor,
        } => conn
            .state
            .forge
            .list_issue_comments(account_id.as_deref(), &repo, number, cursor.as_deref())
            .await
            .map(|page| ResponseBody::ForgeIssueComments { page })
            .map_err(ForgeServiceError::into_proto),
        // Issue writes are audited once, by the forge service that performs
        // them.
        ClientRequest::ForgeCommentIssue {
            account_id,
            repo,
            number,
            body,
        } => {
            if !repo_is_owned_fresh(conn, &repo).await {
                return Err(read_only_repo(&repo));
            }
            conn.state
                .forge
                .comment_issue(account_id.as_deref(), &repo, number, &body)
                .await
                .map(|comment| ResponseBody::ForgeIssueCommentPosted { comment })
                .map_err(ForgeServiceError::into_proto)
        }
        ClientRequest::ForgeUpdateIssue {
            account_id,
            repo,
            number,
            update,
        } => {
            // One fresh ownership check per request: the write and the
            // read-only flag the response carries are the same answer, and a
            // cached read answer must never authorise the write.
            let read_only = !repo_is_owned_fresh(conn, &repo).await;
            if read_only {
                return Err(read_only_repo(&repo));
            }
            conn.state
                .forge
                .update_issue(account_id.as_deref(), &repo, number, &update)
                .await
                .map(|issue| ResponseBody::ForgeIssue { issue, read_only })
                .map_err(ForgeServiceError::into_proto)
        }
        ClientRequest::LinearListAccounts => Ok(ResponseBody::LinearAccounts {
            accounts: conn.state.linear.list_accounts(),
        }),
        ClientRequest::LinearConnect { api_key } => conn
            .state
            .linear
            .connect(&api_key)
            .await
            .map(|account| {
                conn.audit(
                    "linear.connected",
                    Some("linear"),
                    Some(&account.name),
                    json!({}),
                );
                ResponseBody::LinearAccounts {
                    accounts: conn.state.linear.list_accounts(),
                }
            })
            .map_err(ForgeServiceError::into_proto),
        ClientRequest::LinearRemoveAccount { account_id } => {
            let result = conn.state.linear.remove(&account_id);
            if result.is_ok() {
                conn.audit(
                    "linear.removed",
                    Some("linear"),
                    Some(&account_id),
                    json!({}),
                );
            }
            result
                .map(|()| ResponseBody::Ack)
                .map_err(ForgeServiceError::into_proto)
        }
        ClientRequest::LinearListTeams { account_id } => conn
            .state
            .linear
            .teams(&account_id)
            .await
            .map(|teams| ResponseBody::LinearTeams { teams })
            .map_err(ForgeServiceError::into_proto),
        ClientRequest::LinearProjectTeams => Ok(ResponseBody::LinearProjectTeams {
            teams: conn.state.linear.project_teams(),
        }),
        ClientRequest::LinearSetProjectTeam { project_id, team } => {
            let result = conn.state.linear.set_project_team(&project_id, team).await;
            if let Ok(Some(mapping)) = &result {
                conn.audit(
                    "linear.team_set",
                    Some("project"),
                    Some(&project_id),
                    json!({"account": mapping.account_id, "team": mapping.team_key}),
                );
            }
            result
                .map(|_| ResponseBody::LinearProjectTeams {
                    teams: conn.state.linear.project_teams(),
                })
                .map_err(ForgeServiceError::into_proto)
        }
        ClientRequest::LinearListIssues {
            project_id,
            filter,
            cursor,
            query,
        } => {
            let read_only = !project_is_known(conn, &project_id).await;
            conn.state
                .linear
                .list_issues(&project_id, filter, cursor.as_deref(), query.as_deref())
                .await
                .map(|page| ResponseBody::LinearIssues {
                    issues: page.issues,
                    next_cursor: page.next_cursor,
                    read_only,
                })
                .map_err(ForgeServiceError::into_proto)
        }
        ClientRequest::LinearGetIssue {
            account_id,
            issue_id,
            comments_after,
        } => {
            // One read serves both the detail and the ownership check: the
            // issue already carries its team id.
            let issue = conn
                .state
                .linear
                .get_issue(&account_id, &issue_id, comments_after.as_deref())
                .await
                .map_err(ForgeServiceError::into_proto)?;
            let read_only = !linear_team_owned(conn, &account_id, &issue.summary.team_id).await;
            Ok(ResponseBody::LinearIssue { issue, read_only })
        }
        ClientRequest::LinearCommentIssue {
            account_id,
            issue_id,
            body,
        } => {
            if !linear_issue_owned(conn, &account_id, &issue_id).await? {
                return Err(ProtoError::new(
                    h4rn_protocol::ProtoErrorCode::Forbidden,
                    "this Linear team is not attached to a project on this host, so its issues are read-only",
                    "Choose the team for a project in Settings → Source control & issues",
                ));
            }
            let result = conn
                .state
                .linear
                .comment(&account_id, &issue_id, &body)
                .await;
            if result.is_ok() {
                conn.audit(
                    "linear.issue_comment",
                    Some("linear_issue"),
                    Some(&issue_id),
                    json!({"chars": body.len()}),
                );
            }
            result
                .map(|comment| ResponseBody::LinearIssueCommentPosted { comment })
                .map_err(ForgeServiceError::into_proto)
        }
        ClientRequest::LinkIssue {
            thread_id,
            account_id,
            scope,
            issue_id,
        } => conn
            .state
            .issue_links
            .link(&thread_id, account_id.as_deref(), &scope, &issue_id)
            .await
            .map(|issue| ResponseBody::ThreadIssue { issue })
            .map_err(crate::issue_links::IssueLinkError::into_proto),
        ClientRequest::UnlinkIssue {
            thread_id,
            scope,
            issue_id,
        } => conn
            .state
            .issue_links
            .unlink(&thread_id, &scope, &issue_id)
            .await
            .map(|()| ResponseBody::Ack)
            .map_err(crate::issue_links::IssueLinkError::into_proto),
        ClientRequest::ListThreadIssues { thread_id } => conn
            .state
            .issue_links
            .list(&thread_id)
            .await
            .map(|issues| ResponseBody::ThreadIssues { thread_id, issues })
            .map_err(crate::issue_links::IssueLinkError::into_proto),
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

/// Whether a registered project designates this repository for writes.
///
/// An issue read through a repository no local project checks out is shown
/// read-only: the daemon cannot prove the person has write access there. Only
/// the project's first remote counts, so a fork's secondary `upstream` remote
/// never makes the parent writable.
async fn repo_is_owned(conn: &Connection, repo: &h4rn_protocol::ForgeRepoRef) -> bool {
    let projects = conn.state.projects.list().await;
    conn.state.forge.repo_is_designated(&projects, repo).await
}

/// The ownership answer for a write, read from the checkouts again.
///
/// Reads may reuse the brief cache, but a write must see a project that was
/// just removed or whose `origin` just became a fork.
async fn repo_is_owned_fresh(conn: &Connection, repo: &h4rn_protocol::ForgeRepoRef) -> bool {
    let projects = conn.state.projects.list().await;
    conn.state
        .forge
        .repo_is_designated_fresh(&projects, repo)
        .await
}

/// The refusal for a write through a repository no project owns.
fn read_only_repo(repo: &h4rn_protocol::ForgeRepoRef) -> ProtoError {
    ProtoError::new(
        h4rn_protocol::ProtoErrorCode::Forbidden,
        format!(
            "{} is not checked out by a project on this host, so its issues are read-only",
            repo.full_name()
        ),
        "Open the repository as a project on this host to comment or edit",
    )
}

/// Whether a Linear issue's team is attached to a project on this host.
async fn linear_issue_owned(
    conn: &Connection,
    account_id: &str,
    issue_id: &str,
) -> Result<bool, ProtoError> {
    let team_id = conn
        .state
        .linear
        .team_id(account_id, issue_id)
        .await
        .map_err(ForgeServiceError::into_proto)?;
    Ok(linear_team_owned(conn, account_id, &team_id).await)
}

/// Whether a Linear team is attached to a project that still exists.
async fn linear_team_owned(conn: &Connection, account_id: &str, team_id: &str) -> bool {
    let mapping = conn
        .state
        .linear
        .project_teams()
        .into_iter()
        .find(|team| team.account_id == account_id && team.team_id == team_id);
    match mapping {
        Some(team) => project_is_known(conn, &team.project_id).await,
        None => false,
    }
}

/// Whether a registered project still exists.
async fn project_is_known(conn: &Connection, project_id: &str) -> bool {
    conn.state
        .projects
        .list()
        .await
        .iter()
        .any(|project| project.id == project_id)
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

#[cfg(test)]
mod tests {
    use super::read_only_repo;
    use h4rn_protocol::{ForgeKind, ForgeRepoRef, ProtoErrorCode};

    #[test]
    fn a_repository_the_project_does_not_designate_is_refused_for_writes() {
        let repo = ForgeRepoRef {
            forge: ForgeKind::GitHub,
            base_url: "https://github.com".into(),
            owner: "parent".into(),
            repo: "widget".into(),
        };
        let error = read_only_repo(&repo);
        assert_eq!(error.code, ProtoErrorCode::Forbidden);
        assert!(error.message.contains("parent/widget"), "{error:?}");
    }
}
