//! Azure DevOps issues as Boards work items.
//!
//! Azure DevOps has no issue collection separate from work items: the Issues
//! page reads work items of type `Issue` and `Bug` through WIQL, then fetches
//! the fields in one batch. Comments use the work-item comments API; editing
//! uses a JSON Patch request. Tags are exposed as labels. Azure records no
//! close reason, so the capability row says so.

use chrono::Utc;
use h4rn_protocol::{
    ForgeIssue, ForgeIssueComment, ForgeIssueCommentPage, ForgeIssueFilter, ForgeIssueLinkedPr,
    ForgeIssuePage, ForgeIssuePerson, ForgeIssueState, ForgeIssueSummary, ForgeIssueUpdate,
    ForgeLabel, ForgeRepoRef,
};
use serde_json::{Value, json};
use url::form_urlencoded;

use crate::common::{optional_text, parse_datetime, parse_optional_datetime, text, validate_repo};
use crate::error::{ForgeError, Result};
use crate::http::ForgeHttp;

/// Work item types the Issues page treats as issues.
const ISSUE_TYPES: &str = "'Issue', 'Bug'";
/// Rows per page.
const PAGE_SIZE: usize = 50;

fn encode(value: &str) -> String {
    form_urlencoded::byte_serialize(value.as_bytes()).collect()
}

fn project_api(api: &str, repo: &ForgeRepoRef) -> String {
    format!("{}/{}", api.trim_end_matches('/'), encode(&repo.owner))
}

fn wit_query(project: &str) -> String {
    format!("{project}/_apis/wit/wiql?api-version=7.1")
}

async fn fetch_work_items(
    http: &ForgeHttp,
    api: &str,
    repo: &ForgeRepoRef,
    ids: &[u64],
) -> Result<Vec<Value>> {
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    let list = ids.iter().map(u64::to_string).collect::<Vec<_>>().join(",");
    let url = format!(
        "{}/_apis/wit/workitems?ids={list}&fields=System.Id,System.Title,System.State,System.WorkItemType,System.CreatedBy,System.AssignedTo,System.Tags,System.CommentCount,System.CreatedDate,System.ChangedDate,System.Description&api-version=7.1",
        project_api(api, repo)
    );
    let value = http.get_json(&url).await?;
    Ok(value["value"].as_array().cloned().unwrap_or_default())
}

fn query_text(filter: ForgeIssueFilter, search: Option<&str>) -> Result<String> {
    let mut clauses = vec![
        "[System.TeamProject] = @project".to_owned(),
        format!("[System.WorkItemType] IN ({ISSUE_TYPES})"),
    ];
    match filter {
        ForgeIssueFilter::Open | ForgeIssueFilter::Assigned | ForgeIssueFilter::Created => {
            clauses.push("[System.State] NOT IN ('Closed', 'Done', 'Removed')".into());
        }
        ForgeIssueFilter::Closed => {
            clauses.push("[System.State] IN ('Closed', 'Done', 'Removed')".into());
        }
        ForgeIssueFilter::All => {}
        ForgeIssueFilter::Mentioning => {
            return Err(ForgeError::Unsupported(
                "Azure Boards has no issue filter for mentions; use search".into(),
            ));
        }
    }
    match filter {
        ForgeIssueFilter::Assigned => clauses.push("[System.AssignedTo] = @Me".into()),
        ForgeIssueFilter::Created => clauses.push("[System.CreatedBy] = @Me".into()),
        _ => {}
    }
    if let Some(term) = search.map(str::trim).filter(|term| !term.is_empty()) {
        let escaped = term.replace('\'', "''");
        clauses.push(format!("[System.Title] CONTAINS '{escaped}'"));
    }
    Ok(format!(
        "SELECT [System.Id] FROM WorkItems WHERE {} ORDER BY [System.ChangedDate] DESC",
        clauses.join(" AND ")
    ))
}

/// Lists work items that represent issues.
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
    let query = query_text(filter, search)?;
    let project = project_api(api, repo);
    let value = http
        .post_json(&wit_query(&project), &json!({"query": query}))
        .await?;
    let ids = value["workItems"]
        .as_array()
        .map(|items| {
            items
                .iter()
                .filter_map(|item| item["id"].as_u64())
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let start = (page as usize - 1) * PAGE_SIZE;
    let end = (start + PAGE_SIZE).min(ids.len());
    let page_ids = if start < ids.len() {
        &ids[start..end]
    } else {
        &[]
    };
    let items = fetch_work_items(http, api, repo, page_ids).await?;
    Ok(ForgeIssuePage {
        repo: repo.clone(),
        issues: items
            .iter()
            .filter_map(|item| map_summary(repo, item))
            .collect(),
        page,
        has_more: ids.len() > end,
    })
}

/// Reads one work item.
pub(crate) async fn get(
    http: &ForgeHttp,
    api: &str,
    repo: &ForgeRepoRef,
    number: u32,
) -> Result<ForgeIssue> {
    validate_repo(repo)?;
    let url = format!(
        "{}/_apis/wit/workitems/{number}?$expand=relations&api-version=7.1",
        project_api(api, repo)
    );
    let value = http.get_json(&url).await?;
    map_issue(repo, &value)
}

/// Reads one work item's comments.
pub(crate) async fn comments(
    http: &ForgeHttp,
    api: &str,
    repo: &ForgeRepoRef,
    number: u32,
) -> Result<ForgeIssueCommentPage> {
    validate_repo(repo)?;
    let url = format!(
        "{}/_apis/wit/workItems/{number}/comments?api-version=7.1-preview.4",
        project_api(api, repo)
    );
    let value = http.get_json(&url).await?;
    let items = value["comments"].as_array().cloned().unwrap_or_default();
    Ok(ForgeIssueCommentPage {
        repo: repo.clone(),
        number,
        comments: items.iter().filter_map(map_comment).collect(),
        has_more: value["continuationToken"].as_str().is_some(),
    })
}

/// Posts a work item comment.
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
    let url = format!(
        "{}/_apis/wit/workItems/{number}/comments?api-version=7.1-preview.4",
        project_api(api, repo)
    );
    let value = http.post_json(&url, &json!({"text": body})).await?;
    let comment = if value.get("comment").is_some() {
        &value["comment"]
    } else {
        &value
    };
    map_comment(comment).ok_or_else(|| ForgeError::Parse("Azure DevOps returned no comment".into()))
}

/// Applies a partial update through a JSON Patch document.
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
            "Azure Boards does not record a close reason".into(),
        ));
    }
    let mut patch: Vec<Value> = Vec::new();
    if let Some(title) = &update.title {
        patch.push(json!({"op": "add", "path": "/fields/System.Title", "value": title}));
    }
    if let Some(body) = &update.body {
        patch.push(json!({"op": "add", "path": "/fields/System.Description", "value": body}));
    }
    if let Some(state) = update.state {
        patch.push(json!({
            "op": "add",
            "path": "/fields/System.State",
            "value": match state {
                ForgeIssueState::Open => "Active",
                ForgeIssueState::Closed => "Closed",
            },
        }));
    }
    if let Some(labels) = &update.labels {
        patch.push(json!({
            "op": "add",
            "path": "/fields/System.Tags",
            "value": labels.join("; "),
        }));
    }
    if let Some(assignees) = &update.assignees {
        if assignees.len() > 1 {
            return Err(ForgeError::Unsupported(
                "Azure Boards work items have one assignee".into(),
            ));
        }
        patch.push(json!({
            "op": "add",
            "path": "/fields/System.AssignedTo",
            "value": assignees.first().cloned().unwrap_or_default(),
        }));
    }
    let url = format!(
        "{}/_apis/wit/workitems/{number}?api-version=7.1",
        project_api(api, repo)
    );
    http.patch_json_patch(&url, &Value::Array(patch)).await?;
    get(http, api, repo, number).await
}

fn map_issue(repo: &ForgeRepoRef, value: &Value) -> Result<ForgeIssue> {
    let summary = map_summary(repo, value)
        .ok_or_else(|| ForgeError::Parse("Azure DevOps returned an incomplete work item".into()))?;
    let closed = summary.state == ForgeIssueState::Closed;
    let linked_pull_requests = value["relations"]
        .as_array()
        .map(|relations| {
            relations
                .iter()
                .filter(|relation| relation["rel"].as_str() == Some("ArtifactLink"))
                .filter_map(map_linked_pr)
                .collect()
        })
        .unwrap_or_default();
    Ok(ForgeIssue {
        summary,
        body: text(&value["fields"], "System.Description"),
        created_at: parse_datetime(&value["fields"]["System.CreatedDate"], "System.CreatedDate")
            .unwrap_or_else(|_| Utc::now()),
        closed_at: closed
            .then(|| {
                parse_optional_datetime(
                    value["fields"].get("System.ChangedDate"),
                    "System.ChangedDate",
                )
                .ok()
                .flatten()
            })
            .flatten(),
        close_reason: None,
        linked_pull_requests,
    })
}

fn map_summary(repo: &ForgeRepoRef, value: &Value) -> Option<ForgeIssueSummary> {
    let fields = &value["fields"];
    let state = text(fields, "System.State");
    let web_url = optional_text(&value["_links"]["html"], "href").unwrap_or_else(|| {
        format!(
            "{}/{}/_workitems/edit/{}",
            repo.base_url.trim_end_matches('/'),
            encode(&repo.owner),
            value["id"].as_u64().unwrap_or_default()
        )
    });
    Some(ForgeIssueSummary {
        repo: repo.clone(),
        number: value["id"].as_u64()? as u32,
        title: text(fields, "System.Title"),
        state: if matches!(state.as_str(), "Closed" | "Done" | "Removed") {
            ForgeIssueState::Closed
        } else {
            ForgeIssueState::Open
        },
        author: identity(fields.get("System.CreatedBy")),
        labels: tags(fields.get("System.Tags")),
        assignees: fields
            .get("System.AssignedTo")
            .filter(|value| !value.is_null())
            .map(|value| {
                let login = identity(Some(value));
                if login.is_empty() {
                    Vec::new()
                } else {
                    vec![ForgeIssuePerson {
                        login,
                        name: optional_text(value, "displayName"),
                    }]
                }
            })
            .unwrap_or_default(),
        comment_count: fields["System.CommentCount"].as_u64().unwrap_or(0) as u32,
        updated_at: parse_datetime(&fields["System.ChangedDate"], "System.ChangedDate").ok()?,
        web_url,
    })
}

fn identity(value: Option<&Value>) -> String {
    let Some(value) = value else {
        return String::new();
    };
    optional_text(value, "uniqueName")
        .or_else(|| optional_text(value, "displayName"))
        .unwrap_or_default()
}

fn tags(value: Option<&Value>) -> Vec<ForgeLabel> {
    value
        .and_then(Value::as_str)
        .map(|tags| {
            tags.split(';')
                .map(str::trim)
                .filter(|tag| !tag.is_empty())
                .map(|tag| ForgeLabel {
                    name: tag.to_owned(),
                    color: None,
                })
                .collect()
        })
        .unwrap_or_default()
}

fn map_comment(value: &Value) -> Option<ForgeIssueComment> {
    Some(ForgeIssueComment {
        comment_id: value["id"].as_u64()?.to_string(),
        author: identity(value.get("createdBy")),
        body: text(value, "text"),
        created_at: parse_datetime(&value["createdDate"], "createdDate").ok()?,
        updated_at: parse_optional_datetime(value.get("modifiedDate"), "modifiedDate")
            .ok()
            .flatten(),
        web_url: None,
    })
}

fn map_linked_pr(relation: &Value) -> Option<ForgeIssueLinkedPr> {
    let url = optional_text(relation, "url")?;
    let number = url
        .rsplit('/')
        .find_map(|part| part.parse::<u32>().ok())
        .or_else(|| {
            url.rsplit(|character: char| !character.is_ascii_digit())
                .find(|part| !part.is_empty())
                .and_then(|part| part.parse::<u32>().ok())
        })?;
    let web_url = if url.starts_with("http") {
        url.clone()
    } else {
        return None;
    };
    Some(ForgeIssueLinkedPr {
        number,
        title: optional_text(&relation["attributes"], "name")
            .unwrap_or_else(|| format!("Pull request #{number}")),
        state: None,
        web_url,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn closed_states_cover_every_azure_terminal_state() {
        let repo = ForgeRepoRef {
            forge: h4rn_protocol::ForgeKind::AzureDevOps,
            base_url: "https://dev.azure.com/acme".into(),
            owner: "Project".into(),
            repo: "api".into(),
        };
        for state in ["Closed", "Done", "Removed"] {
            let value = json!({
                "id": 3,
                "fields": {
                    "System.State": state,
                    "System.ChangedDate": "2026-01-01T00:00:00Z",
                    "System.CreatedDate": "2026-01-01T00:00:00Z"
                }
            });
            assert_eq!(
                map_summary(&repo, &value).map(|issue| issue.state),
                Some(ForgeIssueState::Closed)
            );
        }
    }

    #[test]
    fn tags_split_into_quiet_labels() {
        let labels = tags(Some(&json!("bug; frontend ")));
        assert_eq!(
            labels
                .iter()
                .map(|label| label.name.as_str())
                .collect::<Vec<_>>(),
            vec!["bug", "frontend"]
        );
    }

    #[test]
    fn wiql_escapes_quotes() {
        let query = query_text(ForgeIssueFilter::Open, Some("it's broken")).unwrap();
        assert!(query.contains("it''s broken"));
    }
}
