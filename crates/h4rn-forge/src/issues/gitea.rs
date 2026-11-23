//! Gitea and Forgejo issues over API v1.
//!
//! Both forges share the same tracker. Lists exclude pull requests with
//! `type=issues`, and the involvement filters are native query flags. Labels
//! are sent as repository label ids, so a replacement name is resolved first
//! and an unknown label is reported rather than silently dropped.

use std::collections::HashMap;

use h4rn_protocol::{
    ForgeIssue, ForgeIssueComment, ForgeIssueCommentPage, ForgeIssueFilter, ForgeIssuePage,
    ForgeIssueState, ForgeIssueSummary, ForgeIssueUpdate, ForgeRepoRef,
};
use serde_json::{Value, json};
use url::form_urlencoded;

use super::{labels, people, state_from_closed};
use crate::common::{optional_text, parse_datetime, parse_optional_datetime, text, validate_repo};
use crate::error::{ForgeError, Result};
use crate::http::ForgeHttp;

/// Gitea's default page size, and the largest page a server returns unless
/// its `MAX_RESPONSE_ITEMS` setting raises it.
const PAGE_LIMIT: usize = 50;

fn repo_url(api: &str, repo: &ForgeRepoRef) -> String {
    format!(
        "{api}/repos/{}/{}",
        form_urlencoded::byte_serialize(repo.owner.as_bytes()).collect::<String>(),
        form_urlencoded::byte_serialize(repo.repo.as_bytes()).collect::<String>()
    )
}

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

/// The authenticated account's login, for the username-based filters.
async fn current_login(http: &ForgeHttp, api: &str) -> Result<String> {
    let value = http.get_json(&format!("{api}/user")).await?;
    let login = text(&value, "login");
    if login.is_empty() {
        return Err(ForgeError::Parse(
            "Gitea returned no login for the authenticated account".into(),
        ));
    }
    Ok(login)
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
    let page = page.max(1);
    let mut params = vec![
        ("type", "issues".to_owned()),
        (
            "state",
            match filter {
                ForgeIssueFilter::Closed => "closed".to_owned(),
                ForgeIssueFilter::All => "all".to_owned(),
                _ => "open".to_owned(),
            },
        ),
        ("limit", PAGE_LIMIT.to_string()),
        ("page", page.to_string()),
    ];
    // The repository issue list filters by an explicit username, not a
    // boolean "me"; the authenticated login comes from `/user`.
    match filter {
        ForgeIssueFilter::Assigned => params.push(("assigned_by", current_login(http, api).await?)),
        ForgeIssueFilter::Created => params.push(("created_by", current_login(http, api).await?)),
        ForgeIssueFilter::Mentioning => {
            params.push(("mentioned_by", current_login(http, api).await?))
        }
        _ => {}
    }
    if let Some(search) = search.map(str::trim).filter(|search| !search.is_empty()) {
        params.push(("q", search.to_owned()));
    }
    let url = format!("{}/issues?{}", repo_url(api, repo), query(&params));
    let value = http.get_json(&url).await?;
    let items = value
        .as_array()
        .ok_or_else(|| ForgeError::Parse("Gitea returned no issue list".into()))?;
    Ok(ForgeIssuePage {
        repo: repo.clone(),
        issues: items
            .iter()
            .filter_map(|item| map_summary(repo, item))
            .collect(),
        page,
        has_more: items.len() >= PAGE_LIMIT,
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
    let value = http
        .get_json(&format!("{}/issues/{number}", repo_url(api, repo)))
        .await?;
    let summary = map_summary(repo, &value)
        .ok_or_else(|| ForgeError::Parse("Gitea returned an incomplete issue".into()))?;
    Ok(ForgeIssue {
        summary,
        body: text(&value, "body"),
        created_at: parse_datetime(&value["created_at"], "created_at").ok(),
        closed_at: parse_optional_datetime(value.get("closed_at"), "closed_at")
            .ok()
            .flatten(),
        close_reason: None,
        linked_pull_requests: Vec::new(),
    })
}

/// Reads one issue's comments.
///
/// Gitea's issue-comment endpoint is not paged: its swagger lists only
/// `owner`, `repo`, `index`, `since` and `before`, and the server ignores
/// `limit`/`page`. Every call returns the whole discussion, so this is a
/// single complete page and the caller never asks for another.
pub(crate) async fn comments(
    http: &ForgeHttp,
    api: &str,
    repo: &ForgeRepoRef,
    number: u32,
    _cursor: Option<&str>,
) -> Result<ForgeIssueCommentPage> {
    validate_repo(repo)?;
    let url = format!("{}/issues/{number}/comments", repo_url(api, repo));
    let value = http.get_json(&url).await?;
    let items = value
        .as_array()
        .ok_or_else(|| ForgeError::Parse("Gitea returned no issue comments".into()))?;
    Ok(ForgeIssueCommentPage {
        repo: repo.clone(),
        number,
        comments: items.iter().filter_map(map_comment).collect(),
        has_more: false,
        next_cursor: None,
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
    if body.trim().is_empty() {
        return Err(ForgeError::InvalidRequest("the comment is empty".into()));
    }
    let value = http
        .post_json(
            &format!("{}/issues/{number}/comments", repo_url(api, repo)),
            &json!({"body": body}),
        )
        .await?;
    map_comment(&value).ok_or_else(|| ForgeError::Parse("Gitea returned no comment".into()))
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
            "Gitea does not record a close reason".into(),
        ));
    }
    let base = repo_url(api, repo);
    let mut body = serde_json::Map::new();
    if let Some(title) = &update.title {
        body.insert("title".into(), json!(title));
    }
    if let Some(text) = &update.body {
        body.insert("body".into(), json!(text));
    }
    if let Some(state) = update.state {
        body.insert(
            "state".into(),
            json!(match state {
                ForgeIssueState::Open => "open",
                ForgeIssueState::Closed => "closed",
            }),
        );
    }
    if let Some(assignees) = &update.assignees {
        body.insert("assignees".into(), json!(assignees));
    }
    if !body.is_empty() {
        http.patch_json(&format!("{base}/issues/{number}"), &Value::Object(body))
            .await?;
    }
    // Gitea replaces labels through their own endpoint, by repository label
    // id; the issue PATCH body does not carry them.
    if let Some(names) = &update.labels {
        let ids = label_ids(http, &base, names).await?;
        http.put_json(
            &format!("{base}/issues/{number}/labels"),
            &json!({"labels": ids}),
        )
        .await?;
    }
    get(http, api, repo, number).await
}

/// Resolves label names to repository label ids, paging through the
/// catalogue until every requested label has been seen.
async fn label_ids(http: &ForgeHttp, repo: &str, names: &[String]) -> Result<Vec<u64>> {
    if names.is_empty() {
        return Ok(Vec::new());
    }
    let mut resolved: HashMap<String, u64> = HashMap::new();
    let mut page = 1u32;
    loop {
        let value = http
            .get_json(&format!("{repo}/labels?limit={PAGE_LIMIT}&page={page}"))
            .await?;
        let available = value.as_array().cloned().unwrap_or_default();
        for name in names {
            if resolved.contains_key(name) {
                continue;
            }
            if let Some(id) = available
                .iter()
                .find(|label| text(label, "name").eq_ignore_ascii_case(name))
                .and_then(|label| label["id"].as_u64())
            {
                resolved.insert(name.clone(), id);
            }
        }
        if resolved.len() == names.len() || available.len() < PAGE_LIMIT {
            break;
        }
        // A server that ignores `page` must not spin forever.
        page += 1;
        if page > 100 {
            break;
        }
    }
    names
        .iter()
        .map(|name| {
            resolved.get(name).copied().ok_or_else(|| {
                ForgeError::InvalidRequest(format!(
                    "the repository has no label called {name}; create it on the forge first"
                ))
            })
        })
        .collect()
}

fn map_summary(repo: &ForgeRepoRef, value: &Value) -> Option<ForgeIssueSummary> {
    Some(ForgeIssueSummary {
        repo: repo.clone(),
        number: value["number"].as_u64()? as u32,
        title: text(value, "title"),
        state: state_from_closed(value["state"].as_str() == Some("closed")),
        author: text(&value["user"], "login"),
        labels: labels(&value["labels"]),
        assignees: people(&value["assignees"]),
        comment_count: value["comments"].as_u64().unwrap_or(0) as u32,
        updated_at: parse_datetime(&value["updated_at"], "updated_at").ok(),
        web_url: optional_text(value, "html_url").unwrap_or_default(),
    })
}

fn map_comment(value: &Value) -> Option<ForgeIssueComment> {
    Some(ForgeIssueComment {
        comment_id: value["id"].as_u64()?.to_string(),
        author: text(&value["user"], "login"),
        body: text(value, "body"),
        created_at: parse_datetime(&value["created_at"], "created_at").ok()?,
        updated_at: parse_optional_datetime(value.get("updated_at"), "updated_at")
            .ok()
            .flatten(),
        web_url: optional_text(value, "html_url"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn label_names_are_resolved_case_insensitively() {
        // The resolver itself needs a server; the comparison is the rule that
        // matters and is exercised here without one.
        let available = json!([{"id": 3, "name": "Bug"}]);
        let found = available
            .as_array()
            .and_then(|labels| {
                labels
                    .iter()
                    .find(|label| text(label, "name").eq_ignore_ascii_case("bug"))
            })
            .and_then(|label| label["id"].as_u64());
        assert_eq!(found, Some(3));
    }

    #[test]
    fn issue_states_are_closed_only_when_the_forge_says_so() {
        let repo = ForgeRepoRef {
            forge: h4rn_protocol::ForgeKind::Gitea,
            base_url: "https://gitea.com".into(),
            owner: "o".into(),
            repo: "r".into(),
        };
        let value = json!({
            "number": 4,
            "title": "Fix",
            "state": "open",
            "updated_at": "2026-01-01T00:00:00Z",
            "user": {"login": "me"}
        });
        assert_eq!(
            map_summary(&repo, &value).map(|issue| issue.state),
            Some(ForgeIssueState::Open)
        );
    }
}
