//! Frames written by a client without the issue feature's paging or duplicate
//! fields still parse, so an older app and a newer daemon (or the reverse)
//! keep working.

use h4rn_protocol::issues::{ForgeIssueCloseReason, ForgeIssueState, ForgeIssueUpdate};
use h4rn_protocol::{ClientFrame, ClientRequest};

fn request(json: &str) -> ClientRequest {
    serde_json::from_str::<ClientFrame>(json)
        .expect("an older frame parses")
        .body
}

const REPO: &str =
    r#"{"forge":"github","base_url":"https://github.com","owner":"acme","repo":"widget"}"#;

#[test]
fn forge_list_issue_comments_without_a_cursor_starts_at_the_first_page() {
    let body = request(&format!(
        r#"{{"id":1,"body":{{"type":"forge_list_issue_comments","account_id":null,"repo":{REPO},"number":12}}}}"#
    ));
    match body {
        ClientRequest::ForgeListIssueComments { number, cursor, .. } => {
            assert_eq!(number, 12);
            assert_eq!(cursor, None);
        }
        other => panic!("unexpected request: {other:?}"),
    }
}

#[test]
fn linear_get_issue_without_comments_after_reads_the_first_comment_page() {
    let body = request(
        r#"{"id":1,"body":{"type":"linear_get_issue","account_id":"lin_1","issue_id":"iss_1"}}"#,
    );
    match body {
        ClientRequest::LinearGetIssue {
            issue_id,
            comments_after,
            ..
        } => {
            assert_eq!(issue_id, "iss_1");
            assert_eq!(comments_after, None);
        }
        other => panic!("unexpected request: {other:?}"),
    }
}

#[test]
fn linear_list_issues_without_cursor_or_query_lists_from_the_start() {
    let body = request(
        r#"{"id":1,"body":{"type":"linear_list_issues","project_id":"proj_1","filter":"open"}}"#,
    );
    match body {
        ClientRequest::LinearListIssues {
            project_id,
            filter,
            cursor,
            query,
        } => {
            assert_eq!(project_id, "proj_1");
            assert_eq!(filter, h4rn_protocol::issues::ForgeIssueFilter::Open);
            assert_eq!(cursor, None);
            assert_eq!(query, None);
        }
        other => panic!("unexpected request: {other:?}"),
    }
}

#[test]
fn forge_update_issue_without_a_duplicate_target_keeps_no_duplicate() {
    let body = request(&format!(
        r#"{{"id":1,"body":{{"type":"forge_update_issue","account_id":null,"repo":{REPO},"number":12,"update":{{"title":"New title","state":"closed","close_reason":"duplicate"}}}}}}"#
    ));
    match body {
        ClientRequest::ForgeUpdateIssue { update, .. } => {
            assert_eq!(update.title.as_deref(), Some("New title"));
            assert_eq!(update.state, Some(ForgeIssueState::Closed));
            assert_eq!(update.close_reason, Some(ForgeIssueCloseReason::Duplicate));
            assert_eq!(update.duplicate_issue_id, None);
            assert!(!update.is_empty());
        }
        other => panic!("unexpected request: {other:?}"),
    }
}

#[test]
fn an_older_issue_update_defaults_every_new_field() {
    let update: ForgeIssueUpdate = serde_json::from_str("{}").expect("an empty update parses");
    assert!(update.is_empty());
    assert_eq!(update.duplicate_issue_id, None);
}
