//! GitHub issues over REST: list, detail, comments, writes and linked PRs.
//!
//! GitHub's `/issues` collection also returns pull requests, so plain lists
//! filter those rows out and every search carries `is:issue`. The timeline is
//! the only REST view that reports the pull requests cross-referenced from an
//! issue, which is why detail costs one extra request.

use h4rn_protocol::{
    ForgeIssue, ForgeIssueCloseReason, ForgeIssueComment, ForgeIssueCommentPage, ForgeIssueFilter,
    ForgeIssueLinkedPr, ForgeIssuePage, ForgeIssueState, ForgeIssueSummary, ForgeIssueUpdate,
    ForgePrState, ForgeRepoRef,
};
use reqwest::Method;
use serde_json::{Value, json};
use url::form_urlencoded;

use super::{labels, people, state_from_closed};
use crate::common::{optional_text, parse_datetime, parse_optional_datetime, text, validate_repo};
use crate::error::{ForgeError, Result};
use crate::http::{ForgeHttp, next_link};

/// One page of results and whether another page exists.
struct Fetched {
    values: Vec<Value>,
    has_more: bool,
}

async fn page_of_issues(http: &ForgeHttp, url: &str, search: bool) -> Result<Fetched> {
    let response = http.request(Method::GET, url, None, false).await?;
    let headers = response.headers.clone();
    let value = http.json(response)?;
    let array = if search {
        value["items"].as_array()
    } else {
        value.as_array()
    }
    .ok_or_else(|| ForgeError::Parse("GitHub returned no issue list".into()))?
    .iter()
    .filter(|item| item.get("pull_request").is_none())
    .cloned()
    .collect::<Vec<_>>();
    let has_more = if search {
        array.len() == 50
    } else {
        next_link(&headers).is_some()
    };
    Ok(Fetched {
        values: array,
        has_more,
    })
}

fn search_qualifier(filter: ForgeIssueFilter) -> Option<&'static str> {
    match filter {
        ForgeIssueFilter::Assigned => Some("assignee:@me"),
        ForgeIssueFilter::Created => Some("author:@me"),
        ForgeIssueFilter::Mentioning => Some("mentions:@me"),
        _ => None,
    }
}

/// Lists issues for one repository.
pub(crate) async fn list(
    http: &ForgeHttp,
    api: &str,
    repo: &ForgeRepoRef,
    filter: ForgeIssueFilter,
    page: u32,
    query: Option<&str>,
) -> Result<ForgeIssuePage> {
    validate_repo(repo)?;
    let page = page.max(1);
    let repo_url = format!(
        "{}/repos/{}/{}",
        api,
        encode(&repo.owner),
        encode(&repo.repo)
    );
    let involvement = search_qualifier(filter);
    let query = query.map(str::trim).filter(|query| !query.is_empty());
    if involvement.is_some() || query.is_some() {
        let mut terms = vec![
            format!("repo:{}/{}", repo.owner, repo.repo),
            "is:issue".into(),
        ];
        match filter {
            ForgeIssueFilter::Closed => terms.push("is:closed".into()),
            // Opened and mentioning are involvement filters like assigned:
            // they answer "my open work", so they close the state too.
            ForgeIssueFilter::All => {}
            _ => terms.push("is:open".into()),
        }
        if let Some(involvement) = involvement {
            terms.push(involvement.into());
        }
        if let Some(query) = query {
            terms.push(query.to_owned());
        }
        let url = format!(
            "{}/search/issues?q={}&per_page=50&page={page}&sort=updated&order=desc",
            api,
            encode(&terms.join(" "))
        );
        let fetched = page_of_issues(http, &url, true).await?;
        return Ok(ForgeIssuePage {
            repo: repo.clone(),
            issues: fetched
                .values
                .iter()
                .filter_map(|value| map_summary(repo, value))
                .collect(),
            page,
            has_more: fetched.has_more,
        });
    }
    let state = match filter {
        ForgeIssueFilter::Closed => "closed",
        ForgeIssueFilter::All => "all",
        _ => "open",
    };
    let url = format!(
        "{repo_url}/issues?state={state}&per_page=50&page={page}&sort=updated&direction=desc"
    );
    let fetched = page_of_issues(http, &url, false).await?;
    Ok(ForgeIssuePage {
        repo: repo.clone(),
        issues: fetched
            .values
            .iter()
            .filter_map(|value| map_summary(repo, value))
            .collect(),
        page,
        has_more: fetched.has_more,
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
    let repo_url = format!(
        "{}/repos/{}/{}",
        api,
        encode(&repo.owner),
        encode(&repo.repo)
    );
    let value = http
        .get_json(&format!("{repo_url}/issues/{number}"))
        .await?;
    if value.get("pull_request").is_some() {
        return Err(ForgeError::NotFound(format!(
            "#{number} is a pull request, not an issue"
        )));
    }
    let summary = map_summary(repo, &value)
        .ok_or_else(|| ForgeError::Parse("GitHub returned an incomplete issue".into()))?;
    let linked_pull_requests = linked_pull_requests(http, &repo_url, number, repo).await;
    Ok(ForgeIssue {
        summary,
        body: text(&value, "body"),
        created_at: parse_datetime(&value["created_at"], "created_at").ok(),
        closed_at: parse_optional_datetime(value.get("closed_at"), "closed_at")
            .ok()
            .flatten(),
        close_reason: close_reason(value["state_reason"].as_str()),
        linked_pull_requests,
    })
}

/// Reads one issue's comments.
///
/// GitHub pages with a `Link` header; the protocol cursor is the next page
/// number, which keeps every provider's continuation a plain opaque string.
pub(crate) async fn comments(
    http: &ForgeHttp,
    api: &str,
    repo: &ForgeRepoRef,
    number: u32,
    cursor: Option<&str>,
) -> Result<ForgeIssueCommentPage> {
    validate_repo(repo)?;
    let repo_url = format!(
        "{}/repos/{}/{}",
        api,
        encode(&repo.owner),
        encode(&repo.repo)
    );
    let page = cursor
        .and_then(|cursor| cursor.parse::<u32>().ok())
        .unwrap_or(1)
        .max(1);
    let url = format!("{repo_url}/issues/{number}/comments?per_page=100&page={page}");
    let response = http.request(Method::GET, &url, None, false).await?;
    let more = next_link(&response.headers).is_some();
    let value = http.json(response)?;
    let values = value
        .as_array()
        .ok_or_else(|| ForgeError::Parse("GitHub returned no issue comments".into()))?;
    Ok(ForgeIssueCommentPage {
        repo: repo.clone(),
        number,
        comments: values.iter().filter_map(map_comment).collect(),
        has_more: more,
        next_cursor: more.then(|| (page + 1).to_string()),
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
    let repo_url = format!(
        "{}/repos/{}/{}",
        api,
        encode(&repo.owner),
        encode(&repo.repo)
    );
    let value = http
        .post_json(
            &format!("{repo_url}/issues/{number}/comments"),
            &json!({"body": body}),
        )
        .await?;
    map_comment(&value).ok_or_else(|| ForgeError::Parse("GitHub returned no comment".into()))
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
    let repo_url = format!(
        "{}/repos/{}/{}",
        api,
        encode(&repo.owner),
        encode(&repo.repo)
    );
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
        if state == ForgeIssueState::Closed
            && let Some(reason) = update.close_reason
        {
            if reason == ForgeIssueCloseReason::Duplicate {
                let duplicate = update.duplicate_issue_id.ok_or_else(|| {
                    ForgeError::InvalidRequest(
                        "closing as a duplicate needs the number of the issue it duplicates".into(),
                    )
                })?;
                // REST names the duplicate by its repository-wide 64-bit id,
                // not the number the person typed into the picker, so the
                // number is resolved to the issue first.
                let picked = http
                    .get_json(&format!("{repo_url}/issues/{duplicate}"))
                    .await?;
                let id = picked["id"].as_u64().ok_or_else(|| {
                    ForgeError::Parse(format!(
                        "GitHub did not report the id of issue #{duplicate}"
                    ))
                })?;
                body.insert("duplicate_issue_id".into(), json!(id));
            }
            body.insert(
                "state_reason".into(),
                json!(match reason {
                    ForgeIssueCloseReason::Completed => "completed",
                    ForgeIssueCloseReason::NotPlanned => "not_planned",
                    ForgeIssueCloseReason::Duplicate => "duplicate",
                }),
            );
        }
    }
    if let Some(labels) = &update.labels {
        body.insert("labels".into(), json!(labels));
    }
    if let Some(assignees) = &update.assignees {
        body.insert("assignees".into(), json!(assignees));
    }
    let value = http
        .patch_json(&format!("{repo_url}/issues/{number}"), &Value::Object(body))
        .await?;
    let summary = map_summary(repo, &value)
        .ok_or_else(|| ForgeError::Parse("GitHub returned an incomplete issue".into()))?;
    let linked_pull_requests = linked_pull_requests(http, &repo_url, number, repo).await;
    Ok(ForgeIssue {
        summary,
        body: text(&value, "body"),
        created_at: parse_datetime(&value["created_at"], "created_at").ok(),
        closed_at: parse_optional_datetime(value.get("closed_at"), "closed_at")
            .ok()
            .flatten(),
        close_reason: close_reason(value["state_reason"].as_str()),
        linked_pull_requests,
    })
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

/// Reads every timeline page that carries a cross-referenced pull request.
///
/// GitHub pages the timeline with a `Link` header; one request only sees the
/// first hundred events, so an older cross-reference would never appear. The
/// walk stops after ten pages even if the server keeps advertising a next
/// one, and a failed later page keeps the links already found. A next link must
/// stay on the API's own origin: a GitHub Enterprise host or a proxy can name
/// another host, and following it would hand that host the account's
/// Authorization header.
const TIMELINE_PAGE_LIMIT: u32 = 10;

async fn linked_pull_requests(
    http: &ForgeHttp,
    repo_url: &str,
    number: u32,
    repo: &ForgeRepoRef,
) -> Vec<ForgeIssueLinkedPr> {
    let mut linked: Vec<ForgeIssueLinkedPr> = Vec::new();
    let mut next = Some(format!("{repo_url}/issues/{number}/timeline?per_page=100"));
    let mut pages = 0;
    while let Some(url) = next {
        if pages >= TIMELINE_PAGE_LIMIT {
            break;
        }
        pages += 1;
        let response = match http.request(Method::GET, &url, None, false).await {
            Ok(response) => response,
            Err(_) => break,
        };
        let following = next_link(&response.headers);
        let Ok(value) = http.json(response) else {
            break;
        };
        for pull in map_linked_prs(repo, &value) {
            // A pull request is identified by its own URL, not by its number:
            // a cross-reference can name a pull request in another repository,
            // whose number may match one already collected here.
            if !linked
                .iter()
                .any(|existing| existing.web_url == pull.web_url)
            {
                linked.push(pull);
            }
        }
        next = following.filter(|next| {
            if crate::http::same_origin(next, repo_url) {
                return true;
            }
            tracing::warn!(
                next = next.as_str(),
                "GitHub's timeline pointed at another origin; stopped paging"
            );
            false
        });
    }
    linked
}

fn map_linked_prs(repo: &ForgeRepoRef, value: &Value) -> Vec<ForgeIssueLinkedPr> {
    let mut linked: Vec<ForgeIssueLinkedPr> = Vec::new();
    for event in value.as_array().into_iter().flatten() {
        if event["event"].as_str() != Some("cross-referenced") {
            continue;
        }
        let source = &event["source"]["issue"];
        if source.get("pull_request").is_none() {
            continue;
        }
        let Some(number) = source["number"].as_u64().map(|number| number as u32) else {
            continue;
        };
        let web_url = optional_text(source, "html_url").unwrap_or_else(|| repo.pr_web_url(number));
        if linked.iter().any(|pr| pr.web_url == web_url) {
            continue;
        }
        linked.push(ForgeIssueLinkedPr {
            number,
            title: text(source, "title"),
            state: Some(if source["pull_request"]["merged_at"].as_str().is_some() {
                ForgePrState::Merged
            } else if source["state"].as_str() == Some("closed") {
                ForgePrState::Closed
            } else {
                ForgePrState::Open
            }),
            web_url,
        });
    }
    linked
}

fn close_reason(value: Option<&str>) -> Option<ForgeIssueCloseReason> {
    match value {
        Some("completed") => Some(ForgeIssueCloseReason::Completed),
        Some("not_planned") => Some(ForgeIssueCloseReason::NotPlanned),
        Some("duplicate") => Some(ForgeIssueCloseReason::Duplicate),
        _ => None,
    }
}

fn encode(value: &str) -> String {
    form_urlencoded::byte_serialize(value.as_bytes()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_cross_referenced_pull_requests_are_linked() {
        let repo = ForgeRepoRef {
            forge: h4rn_protocol::ForgeKind::GitHub,
            base_url: "https://github.com".into(),
            owner: "o".into(),
            repo: "r".into(),
        };
        let value = json!([
            {"event": "cross-referenced", "source": {"issue": {"number": 7, "title": "Fix", "state": "open", "html_url": "https://github.com/o/r/pull/7", "pull_request": {"merged_at": null}}}},
            {"event": "cross-referenced", "source": {"issue": {"number": 9, "title": "Issue", "state": "open"}}},
            {"event": "labeled"}
        ]);
        let linked = map_linked_prs(&repo, &value);
        assert_eq!(linked.len(), 1);
        assert_eq!(linked[0].number, 7);
        assert_eq!(linked[0].state, Some(ForgePrState::Open));
    }
}
