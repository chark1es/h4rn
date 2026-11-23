//! GitLab issues over REST v4.
//!
//! Labels, assignees and notes are ordinary issue fields. Closing uses
//! `state_event`; GitLab records no close reason, so the capability row says
//! so and the daemon never sends one. Assignee updates need member ids, which
//! are resolved by exact username before the issue is patched.

use chrono::Utc;
use h4rn_protocol::{
    ForgeIssue, ForgeIssueComment, ForgeIssueCommentPage, ForgeIssueFilter, ForgeIssueLinkedPr,
    ForgeIssuePage, ForgeIssuePerson, ForgeIssueState, ForgeIssueSummary, ForgeIssueUpdate,
    ForgePrState, ForgeRepoRef,
};
use serde_json::{Value, json};
use url::form_urlencoded;

use super::state_from_closed;
use crate::common::{
    encode_project_path, optional_text, parse_datetime, parse_optional_datetime, text,
    validate_repo,
};
use crate::error::{ForgeError, Result};
use crate::http::ForgeHttp;

fn query(pairs: &[(&str, String)]) -> String {
    pairs
        .iter()
        .map(|(key, value)| {
            format!(
                "{}={}",
                key,
                form_urlencoded::byte_serialize(value.as_bytes()).collect::<String>()
            )
        })
        .collect::<Vec<_>>()
        .join("&")
}

fn project_url(api: &str, repo: &ForgeRepoRef) -> String {
    format!("{api}/projects/{}", encode_project_path(repo))
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
    if filter == ForgeIssueFilter::Mentioning {
        return Err(ForgeError::Unsupported(
            "GitLab's issue API has no mentions filter; use search".into(),
        ));
    }
    let page = page.max(1);
    let mut params = vec![
        (
            "state",
            match filter {
                ForgeIssueFilter::Closed => "closed".to_owned(),
                ForgeIssueFilter::All => "all".to_owned(),
                _ => "opened".to_owned(),
            },
        ),
        ("per_page", "50".to_owned()),
        ("page", page.to_string()),
        ("order_by", "updated_at".to_owned()),
        ("sort", "desc".to_owned()),
    ];
    match filter {
        ForgeIssueFilter::Assigned => params.push(("scope", "assigned_to_me".to_owned())),
        ForgeIssueFilter::Created => params.push(("scope", "created_by_me".to_owned())),
        _ => {}
    }
    if let Some(search) = search.map(str::trim).filter(|search| !search.is_empty()) {
        params.push(("search", search.to_owned()));
    }
    let url = format!("{}/issues?{}", project_url(api, repo), query(&params));
    let value = http.get_json(&url).await?;
    let items = value
        .as_array()
        .ok_or_else(|| ForgeError::Parse("GitLab returned no issue list".into()))?;
    Ok(ForgeIssuePage {
        repo: repo.clone(),
        issues: items
            .iter()
            .filter_map(|item| map_summary(repo, item))
            .collect(),
        page,
        has_more: items.len() == 50,
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
    let base = project_url(api, repo);
    let value = http.get_json(&format!("{base}/issues/{number}")).await?;
    let summary = map_summary(repo, &value)
        .ok_or_else(|| ForgeError::Parse("GitLab returned an incomplete issue".into()))?;
    let linked_pull_requests = linked_pull_requests(http, &base, repo, number).await;
    Ok(ForgeIssue {
        summary,
        body: text(&value, "description"),
        created_at: parse_datetime(&value["created_at"], "created_at")
            .unwrap_or_else(|_| Utc::now()),
        closed_at: parse_optional_datetime(value.get("closed_at"), "closed_at")
            .ok()
            .flatten(),
        close_reason: None,
        linked_pull_requests,
    })
}

/// Reads one issue's notes.
pub(crate) async fn comments(
    http: &ForgeHttp,
    api: &str,
    repo: &ForgeRepoRef,
    number: u32,
) -> Result<ForgeIssueCommentPage> {
    validate_repo(repo)?;
    let url = format!(
        "{}/issues/{number}/notes?per_page=100&page=1&sort=asc",
        project_url(api, repo)
    );
    let value = http.get_json(&url).await?;
    let items = value
        .as_array()
        .ok_or_else(|| ForgeError::Parse("GitLab returned no issue notes".into()))?;
    Ok(ForgeIssueCommentPage {
        repo: repo.clone(),
        number,
        comments: items
            .iter()
            .filter(|note| !note["system"].as_bool().unwrap_or(false))
            .filter_map(map_comment)
            .collect(),
        has_more: items.len() == 100,
    })
}

/// Posts a note.
pub(crate) async fn comment(
    http: &ForgeHttp,
    api: &str,
    repo: &ForgeRepoRef,
    number: u32,
    body: &str,
) -> Result<ForgeIssueComment> {
    validate_repo(repo)?;
    if body.trim().is_empty() {
        return Err(ForgeError::InvalidRequest("the comment is empty".into()));
    }
    let value = http
        .post_json(
            &format!("{}/issues/{number}/notes", project_url(api, repo)),
            &json!({"body": body}),
        )
        .await?;
    map_comment(&value).ok_or_else(|| ForgeError::Parse("GitLab returned no note".into()))
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
    if update.is_empty() {
        return Err(ForgeError::InvalidRequest(
            "the issue update changes nothing".into(),
        ));
    }
    if update.close_reason.is_some() {
        return Err(ForgeError::Unsupported(
            "GitLab does not record a close reason".into(),
        ));
    }
    let base = project_url(api, repo);
    let mut body = serde_json::Map::new();
    if let Some(title) = &update.title {
        body.insert("title".into(), json!(title));
    }
    if let Some(text) = &update.body {
        body.insert("description".into(), json!(text));
    }
    if let Some(state) = update.state {
        body.insert(
            "state_event".into(),
            json!(match state {
                ForgeIssueState::Open => "reopen",
                ForgeIssueState::Closed => "close",
            }),
        );
    }
    if let Some(labels) = &update.labels {
        body.insert("labels".into(), json!(labels.join(",")));
    }
    if let Some(assignees) = &update.assignees {
        let mut ids = Vec::with_capacity(assignees.len());
        for login in assignees {
            ids.push(member_id(http, &base, login).await?);
        }
        body.insert("assignee_ids".into(), json!(ids));
    }
    let value = http
        .put_json(&format!("{base}/issues/{number}"), &Value::Object(body))
        .await?;
    let summary = map_summary(repo, &value)
        .ok_or_else(|| ForgeError::Parse("GitLab returned an incomplete issue".into()))?;
    let linked_pull_requests = linked_pull_requests(http, &base, repo, number).await;
    Ok(ForgeIssue {
        summary,
        body: text(&value, "description"),
        created_at: parse_datetime(&value["created_at"], "created_at")
            .unwrap_or_else(|_| Utc::now()),
        closed_at: parse_optional_datetime(value.get("closed_at"), "closed_at")
            .ok()
            .flatten(),
        close_reason: None,
        linked_pull_requests,
    })
}

async fn linked_pull_requests(
    http: &ForgeHttp,
    project: &str,
    repo: &ForgeRepoRef,
    number: u32,
) -> Vec<ForgeIssueLinkedPr> {
    http.get_json(&format!(
        "{project}/issues/{number}/related_merge_requests?per_page=20"
    ))
    .await
    .map(|value| {
        value
            .as_array()
            .map(|items| {
                items
                    .iter()
                    .filter_map(|item| {
                        let number = item["iid"].as_u64()? as u32;
                        Some(ForgeIssueLinkedPr {
                            number,
                            title: text(item, "title"),
                            state: Some(pr_state(item["state"].as_str())),
                            web_url: optional_text(item, "web_url")
                                .unwrap_or_else(|| repo.pr_web_url(number)),
                        })
                    })
                    .collect()
            })
            .unwrap_or_default()
    })
    .unwrap_or_default()
}

async fn member_id(http: &ForgeHttp, project: &str, login: &str) -> Result<u64> {
    let value = http
        .get_json(&format!(
            "{project}/members/all?per_page=100&query={}",
            form_urlencoded::byte_serialize(login.as_bytes()).collect::<String>()
        ))
        .await?;
    value
        .as_array()
        .and_then(|members| {
            members
                .iter()
                .find(|member| text(member, "username").eq_ignore_ascii_case(login))
        })
        .and_then(|member| member["id"].as_u64())
        .ok_or_else(|| {
            ForgeError::InvalidRequest(format!(
                "GitLab has no project member called {login}; add them to the project first"
            ))
        })
}

fn map_summary(repo: &ForgeRepoRef, value: &Value) -> Option<ForgeIssueSummary> {
    Some(ForgeIssueSummary {
        repo: repo.clone(),
        number: value["iid"].as_u64()? as u32,
        title: text(value, "title"),
        state: state_from_closed(value["state"].as_str() == Some("closed")),
        author: text(&value["author"], "username"),
        labels: label_names(&value["labels"]),
        assignees: value["assignees"]
            .as_array()
            .map(|people| {
                people
                    .iter()
                    .filter_map(|person| {
                        Some(ForgeIssuePerson {
                            login: optional_text(person, "username")?,
                            name: optional_text(person, "name"),
                        })
                    })
                    .collect()
            })
            .unwrap_or_default(),
        comment_count: value["user_notes_count"].as_u64().unwrap_or(0) as u32,
        updated_at: parse_datetime(&value["updated_at"], "updated_at").ok()?,
        web_url: optional_text(value, "web_url").unwrap_or_default(),
    })
}

/// GitLab labels are plain strings.
fn label_names(value: &Value) -> Vec<h4rn_protocol::ForgeLabel> {
    value
        .as_array()
        .map(|labels| {
            labels
                .iter()
                .filter_map(|label| label.as_str())
                .map(|name| h4rn_protocol::ForgeLabel {
                    name: name.to_owned(),
                    color: None,
                })
                .collect()
        })
        .unwrap_or_default()
}

fn map_comment(value: &Value) -> Option<ForgeIssueComment> {
    Some(ForgeIssueComment {
        comment_id: value["id"].as_u64()?.to_string(),
        author: text(&value["author"], "username"),
        body: text(value, "body"),
        created_at: parse_datetime(&value["created_at"], "created_at").ok()?,
        updated_at: parse_optional_datetime(value.get("updated_at"), "updated_at")
            .ok()
            .flatten(),
        web_url: None,
    })
}

fn pr_state(value: Option<&str>) -> ForgePrState {
    match value {
        Some("merged") => ForgePrState::Merged,
        Some("closed") => ForgePrState::Closed,
        _ => ForgePrState::Open,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_gitlab_label_strings_survive() {
        let labels = label_names(&json!(["bug", "frontend"]));
        assert_eq!(
            labels
                .iter()
                .map(|label| label.name.as_str())
                .collect::<Vec<_>>(),
            vec!["bug", "frontend"]
        );
    }

    #[test]
    fn merge_request_states_map_honestly() {
        assert_eq!(pr_state(Some("merged")), ForgePrState::Merged);
        assert_eq!(pr_state(Some("closed")), ForgePrState::Closed);
        assert_eq!(pr_state(Some("opened")), ForgePrState::Open);
    }
}
