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
use crate::http::{ForgeHttp, header_u64, next_link};

/// Gitea's default page size, and the largest page a server returns unless
/// its `MAX_RESPONSE_ITEMS` setting raises it.
const PAGE_LIMIT: usize = 50;

/// Whether a list response has another page.
///
/// Gitea clamps `limit` to the server's `MAX_RESPONSE_ITEMS`, which can be
/// smaller than the page this client asked for. A short page is therefore not
/// proof that the listing ended: a `Link` header with `rel="next"` is
/// authoritative, and the total count bounds the listing when the server sends
/// it. The count alone cannot say which page size the server actually used, so
/// it can ask for one page past the end but never stops before one; with
/// neither header, only an empty page ends the listing.
fn page_has_more(headers: &reqwest::header::HeaderMap, page: u32, returned: usize) -> bool {
    if next_link(headers).is_some() {
        return true;
    }
    if let Some(total) = header_u64(headers, "x-total-count") {
        let seen = u64::from(page) * returned as u64;
        return returned > 0 && seen < total;
    }
    returned > 0
}

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
    let response = http
        .request(reqwest::Method::GET, &url, None, false)
        .await?;
    let headers = response.headers.clone();
    let value = http.json(response)?;
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
        has_more: page_has_more(&headers, page, items.len()),
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
        let ids = label_ids(http, api, repo, number, names).await?;
        http.put_json(
            &format!("{base}/issues/{number}/labels"),
            &json!({"labels": ids}),
        )
        .await?;
    }
    get(http, api, repo, number).await
}

/// Resolves the names in a label replacement to the ids Gitea accepts.
///
/// The labels already on the issue come from the issue's own label list,
/// which a repository-scoped token can read. That matters because a label may
/// belong to the organization that owns the repository, and h4rn's documented
/// token scopes are repository-only: `GET /orgs/{org}/labels` needs an
/// organization scope. Names that are not already applied are looked up in
/// the repository catalogue and, for names it does not carry, the
/// organization's catalogue. A `403` from the organization catalogue leaves
/// repository labels resolved and names the organization labels that could
/// not be found; no replacement is sent.
async fn label_ids(
    http: &ForgeHttp,
    api: &str,
    repo: &ForgeRepoRef,
    number: u32,
    names: &[String],
) -> Result<Vec<u64>> {
    if names.is_empty() {
        return Ok(Vec::new());
    }
    let mut resolved: HashMap<String, u64> = HashMap::new();
    let applied = http
        .get_json(&format!("{}/issues/{number}/labels", repo_url(api, repo)))
        .await?;
    for label in applied.as_array().into_iter().flatten() {
        let Some(id) = label["id"].as_u64() else {
            continue;
        };
        let name = text(label, "name");
        for requested in names {
            if requested.eq_ignore_ascii_case(&name) {
                resolved.entry(requested.clone()).or_insert(id);
            }
        }
    }
    if resolved.len() < names.len() {
        read_label_catalogue(
            http,
            &format!("{}/labels", repo_url(api, repo)),
            names,
            &mut resolved,
        )
        .await?;
    }
    if resolved.len() < names.len() {
        // A repository whose owner is not an organization answers 404; that
        // means there is no second catalogue, not that the label request
        // failed.
        let org = format!(
            "{api}/orgs/{}/labels",
            form_urlencoded::byte_serialize(repo.owner.as_bytes()).collect::<String>()
        );
        read_label_catalogue(http, &org, names, &mut resolved).await?;
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

/// Reads one label catalogue until every requested name resolves or the
/// catalogue ends. Repository labels win: a name already resolved is not
/// looked up again.
///
/// A catalogue may need a scope the token does not have: `GET
/// /orgs/{org}/labels` answers `403` for a repository-only token. Repository
/// labels keep working because the issue's own label list and the repository
/// catalogue are read first; the organization labels are left out of the
/// lookup, so a requested name that only the organization carries cannot be
/// found. That is reported by naming the missing labels and sending no
/// replacement, so the person is told which label to create rather than shown
/// a permission error with no next step. A `404` from the organization
/// catalogue means the owner is not an organization.
async fn read_label_catalogue(
    http: &ForgeHttp,
    catalog: &str,
    names: &[String],
    resolved: &mut HashMap<String, u64>,
) -> Result<()> {
    let mut page = 1u32;
    let mut seen = 0u64;
    loop {
        let url = format!("{catalog}?limit={PAGE_LIMIT}&page={page}");
        let response = http
            .request(reqwest::Method::GET, &url, None, false)
            .await?;
        if response.status == reqwest::StatusCode::NOT_FOUND {
            return Ok(());
        }
        if response.status == reqwest::StatusCode::FORBIDDEN && catalog.contains("/orgs/") {
            let missing = names
                .iter()
                .filter(|name| !resolved.contains_key(*name))
                .cloned()
                .collect::<Vec<_>>()
                .join(", ");
            return Err(ForgeError::InvalidRequest(format!(
                "the token cannot read the organization's label catalogue, so {missing} could not be found; create it on the repository or connect an account with organization access"
            )));
        }
        let total = header_u64(&response.headers, "x-total-count");
        let value = http.json(response)?;
        let available = value.as_array().cloned().unwrap_or_default();
        seen += available.len() as u64;
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
        // The server may clamp `limit`, so a short page is not the last one.
        // Keep going until the catalogue is exhausted: the total count says
        // when that is, or an empty page when the server sends no count.
        if resolved.len() == names.len()
            || available.is_empty()
            || total.is_some_and(|total| seen >= total)
        {
            break;
        }
        // A server that ignores `page` must not spin forever.
        page += 1;
        if page > 100 {
            break;
        }
    }
    Ok(())
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
