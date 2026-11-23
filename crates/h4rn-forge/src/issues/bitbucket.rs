//! Bitbucket Cloud issues (API 2.0).
//!
//! Bitbucket keeps issue trackers per repository and lets an owner disable
//! one. [`list`] reads the repository first and reports a disabled tracker as
//! unsupported instead of an empty list. Bitbucket has no labels, so the
//! capability row hides them and an update that names labels is refused.

use chrono::Utc;
use h4rn_protocol::{
    ForgeIssue, ForgeIssueCloseReason, ForgeIssueComment, ForgeIssueCommentPage, ForgeIssueFilter,
    ForgeIssuePage, ForgeIssuePerson, ForgeIssueState, ForgeIssueSummary, ForgeIssueUpdate,
    ForgeRepoRef,
};
use serde_json::{Value, json};
use url::form_urlencoded;

use super::state_from_closed;
use crate::common::{optional_text, parse_datetime, parse_optional_datetime, text, validate_repo};
use crate::error::{ForgeError, Result};
use crate::http::ForgeHttp;

fn encode(value: &str) -> String {
    form_urlencoded::byte_serialize(value.as_bytes()).collect()
}

fn repo_url(api: &str, repo: &ForgeRepoRef) -> String {
    format!(
        "{}/repositories/{}/{}",
        api,
        encode(&repo.owner),
        encode(&repo.repo)
    )
}

fn open_clause() -> String {
    ["status=\"new\"", "status=\"open\"", "status=\"on hold\""].join(" OR ")
}

fn closed_clause() -> String {
    [
        "status=\"closed\"",
        "status=\"resolved\"",
        "status=\"invalid\"",
        "status=\"duplicate\"",
        "status=\"wontfix\"",
    ]
    .join(" OR ")
}

async fn ensure_tracker(http: &ForgeHttp, api: &str, repo: &ForgeRepoRef) -> Result<()> {
    let value = http.get_json(&repo_url(api, repo)).await?;
    if value["has_issues"].as_bool() == Some(false) {
        return Err(ForgeError::Unsupported(
            "this repository's issue tracker is disabled; enable it in Bitbucket's repository settings"
                .into(),
        ));
    }
    Ok(())
}

/// Lists issues.
pub(crate) async fn list(
    http: &ForgeHttp,
    api: &str,
    repo: &ForgeRepoRef,
    filter: ForgeIssueFilter,
    page: u32,
    search: Option<&str>,
) -> Result<ForgeIssuePage> {
    validate_repo(repo)?;
    ensure_tracker(http, api, repo).await?;
    match filter {
        ForgeIssueFilter::Assigned | ForgeIssueFilter::Created | ForgeIssueFilter::Mentioning => {
            return Err(ForgeError::Unsupported(
                "Bitbucket's issue API has no involvement filter".into(),
            ));
        }
        _ => {}
    }
    let page = page.max(1);
    let state = match filter {
        ForgeIssueFilter::Closed => closed_clause(),
        ForgeIssueFilter::All => String::new(),
        _ => open_clause(),
    };
    let mut clauses = Vec::new();
    if !state.is_empty() {
        clauses.push(format!("({state})"));
    }
    if let Some(term) = search.map(str::trim).filter(|term| !term.is_empty()) {
        clauses.push(format!("(title~\"{}\")", term.replace('"', "")));
    }
    let q = (!clauses.is_empty()).then(|| clauses.join(" AND "));
    let url = match q {
        Some(q) => format!(
            "{}/issues?pagelen=50&page={page}&q={}",
            repo_url(api, repo),
            encode(&q)
        ),
        None => format!("{}/issues?pagelen=50&page={page}", repo_url(api, repo)),
    };
    let value = http.get_json(&url).await?;
    let items = value["values"]
        .as_array()
        .ok_or_else(|| ForgeError::Parse("Bitbucket returned no issue list".into()))?;
    Ok(ForgeIssuePage {
        repo: repo.clone(),
        issues: items
            .iter()
            .filter_map(|item| map_summary(repo, item))
            .collect(),
        page,
        has_more: value["next"].as_str().is_some(),
    })
}

/// Reads one issue.
pub(crate) async fn get(
    http: &ForgeHttp,
    api: &str,
    repo: &ForgeRepoRef,
    number: u32,
) -> Result<ForgeIssue> {
    validate_repo(repo)?;
    ensure_tracker(http, api, repo).await?;
    let value = http
        .get_json(&format!("{}/issues/{number}", repo_url(api, repo)))
        .await?;
    map_issue(repo, &value)
}

/// Reads one issue's comments.
pub(crate) async fn comments(
    http: &ForgeHttp,
    api: &str,
    repo: &ForgeRepoRef,
    number: u32,
) -> Result<ForgeIssueCommentPage> {
    validate_repo(repo)?;
    ensure_tracker(http, api, repo).await?;
    let value = http
        .get_json(&format!(
            "{}/issues/{number}/comments?pagelen=100&page=1",
            repo_url(api, repo)
        ))
        .await?;
    let items = value["values"]
        .as_array()
        .ok_or_else(|| ForgeError::Parse("Bitbucket returned no issue comments".into()))?;
    Ok(ForgeIssueCommentPage {
        repo: repo.clone(),
        number,
        comments: items.iter().filter_map(map_comment).collect(),
        has_more: value["next"].as_str().is_some(),
    })
}

/// Posts a comment.
pub(crate) async fn comment(
    http: &ForgeHttp,
    api: &str,
    repo: &ForgeRepoRef,
    number: u32,
    body: &str,
) -> Result<ForgeIssueComment> {
    validate_repo(repo)?;
    ensure_tracker(http, api, repo).await?;
    if body.trim().is_empty() {
        return Err(ForgeError::InvalidRequest("the comment is empty".into()));
    }
    let value = http
        .post_json(
            &format!("{}/issues/{number}/comments", repo_url(api, repo)),
            &json!({"content": {"raw": body}}),
        )
        .await?;
    map_comment(&value).ok_or_else(|| ForgeError::Parse("Bitbucket returned no comment".into()))
}

/// Applies a partial update.
pub(crate) async fn update(
    http: &ForgeHttp,
    api: &str,
    repo: &ForgeRepoRef,
    number: u32,
    update: &ForgeIssueUpdate,
) -> Result<ForgeIssue> {
    validate_repo(repo)?;
    ensure_tracker(http, api, repo).await?;
    if update.is_empty() {
        return Err(ForgeError::InvalidRequest(
            "the issue update changes nothing".into(),
        ));
    }
    if update.labels.is_some() {
        return Err(ForgeError::Unsupported(
            "Bitbucket issues have no labels; use kind, priority or a component".into(),
        ));
    }
    let current = http
        .get_json(&format!("{}/issues/{number}", repo_url(api, repo)))
        .await?;
    let mut body = serde_json::Map::new();
    body.insert(
        "title".into(),
        json!(
            update
                .title
                .clone()
                .unwrap_or_else(|| text(&current, "title"))
        ),
    );
    let content = update
        .body
        .clone()
        .unwrap_or_else(|| text(&current["content"], "raw"));
    body.insert("content".into(), json!({"raw": content}));
    if let Some(assignees) = &update.assignees {
        if assignees.len() > 1 {
            return Err(ForgeError::Unsupported(
                "Bitbucket issues have one assignee".into(),
            ));
        }
        body.insert(
            "assignee".into(),
            match assignees.first() {
                Some(login) => json!({"nickname": login}),
                None => Value::Null,
            },
        );
    }
    if let Some(state) = update.state {
        let status = match state {
            ForgeIssueState::Open => "open".to_owned(),
            ForgeIssueState::Closed => match update.close_reason {
                Some(ForgeIssueCloseReason::Completed) => "resolved".to_owned(),
                Some(ForgeIssueCloseReason::NotPlanned) => "wontfix".to_owned(),
                Some(ForgeIssueCloseReason::Duplicate) => "duplicate".to_owned(),
                None => "closed".to_owned(),
            },
        };
        body.insert("status".into(), json!(status));
    }
    let value = http
        .put_json(
            &format!("{}/issues/{number}", repo_url(api, repo)),
            &Value::Object(body),
        )
        .await?;
    map_issue(repo, &value)
}

fn map_issue(repo: &ForgeRepoRef, value: &Value) -> Result<ForgeIssue> {
    let summary = map_summary(repo, value)
        .ok_or_else(|| ForgeError::Parse("Bitbucket returned an incomplete issue".into()))?;
    let close_reason = match text(value, "status").as_str() {
        "resolved" => Some(ForgeIssueCloseReason::Completed),
        "wontfix" => Some(ForgeIssueCloseReason::NotPlanned),
        "duplicate" => Some(ForgeIssueCloseReason::Duplicate),
        _ => None,
    };
    Ok(ForgeIssue {
        summary,
        body: text(&value["content"], "raw"),
        created_at: parse_datetime(&value["created_on"], "created_on")
            .unwrap_or_else(|_| Utc::now()),
        closed_at: parse_optional_datetime(value.get("updated_on"), "updated_on")
            .ok()
            .flatten()
            .filter(|_| {
                state_from_closed(is_closed(text(value, "status").as_str()))
                    == ForgeIssueState::Closed
            }),
        close_reason,
        linked_pull_requests: Vec::new(),
    })
}

fn map_summary(repo: &ForgeRepoRef, value: &Value) -> Option<ForgeIssueSummary> {
    let status = text(value, "status");
    let assignee = &value["assignee"];
    let assignees = if assignee.is_null() {
        Vec::new()
    } else {
        optional_text(assignee, "nickname")
            .or_else(|| optional_text(assignee, "display_name"))
            .map(|login| {
                vec![ForgeIssuePerson {
                    login,
                    name: optional_text(assignee, "display_name"),
                }]
            })
            .unwrap_or_default()
    };
    Some(ForgeIssueSummary {
        repo: repo.clone(),
        number: value["id"].as_u64()? as u32,
        title: text(value, "title"),
        state: state_from_closed(is_closed(&status)),
        author: optional_text(&value["reporter"], "nickname")
            .or_else(|| optional_text(&value["reporter"], "display_name"))
            .unwrap_or_default(),
        labels: Vec::new(),
        assignees,
        comment_count: value["comment_count"].as_u64().unwrap_or(0) as u32,
        updated_at: parse_datetime(&value["updated_on"], "updated_on").ok()?,
        web_url: optional_text(&value["links"]["html"], "href").unwrap_or_default(),
    })
}

fn map_comment(value: &Value) -> Option<ForgeIssueComment> {
    Some(ForgeIssueComment {
        comment_id: value["id"].as_u64()?.to_string(),
        author: optional_text(&value["user"], "nickname")
            .or_else(|| optional_text(&value["user"], "display_name"))
            .unwrap_or_default(),
        body: text(&value["content"], "raw"),
        created_at: parse_datetime(&value["created_on"], "created_on").ok()?,
        updated_at: parse_optional_datetime(value.get("updated_on"), "updated_on")
            .ok()
            .flatten(),
        web_url: optional_text(&value["links"]["html"], "href"),
    })
}

fn is_closed(status: &str) -> bool {
    matches!(
        status,
        "closed" | "resolved" | "invalid" | "duplicate" | "wontfix"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn closed_statuses_become_closed_issues() {
        assert!(is_closed("resolved"));
        assert!(is_closed("wontfix"));
        assert!(!is_closed("open"));
        assert!(!is_closed("on hold"));
    }

    #[test]
    fn query_clauses_name_every_status_family() {
        assert!(open_clause().contains("status=\"open\""));
        assert!(closed_clause().contains("status=\"wontfix\""));
    }
}
