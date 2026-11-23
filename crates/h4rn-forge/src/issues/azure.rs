//! Azure DevOps issues as Boards work items.
//!
//! Azure DevOps has no issue collection separate from work items: the Issues
//! page reads work items of type `Issue` and `Bug` through WIQL, then fetches
//! the fields in one batch. Comments use the work-item comments API; editing
//! uses a JSON Patch request. Tags are exposed as labels. Azure records no
//! close reason, so the capability row says so.

use std::collections::HashMap;

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
const ISSUE_TYPES: [&str; 2] = ["Issue", "Bug"];
/// Rows per page.
const PAGE_SIZE: usize = 50;
/// Category Azure gives a finished state.
const COMPLETED: &str = "Completed";
/// Category Azure gives a state that is no longer finished. The API spells
/// it as one word.
const IN_PROGRESS: &str = "InProgress";
/// Category for a state that has not started.
const PROPOSED: &str = "Proposed";

fn encode(value: &str) -> String {
    form_urlencoded::byte_serialize(value.as_bytes()).collect()
}

fn project_api(api: &str, repo: &ForgeRepoRef) -> String {
    format!("{}/{}", api.trim_end_matches('/'), encode(&repo.owner))
}

fn wit_query(project: &str) -> String {
    format!("{project}/_apis/wit/wiql?api-version=7.1")
}

/// Each work item type's states, grouped by the category Azure assigns.
///
/// Scrum and Basic name the finished state `Done`, not `Closed`; the category,
/// not the name, is what says whether a state closes the item.
#[derive(Clone, Debug, Default)]
struct StateCategories {
    by_name: HashMap<String, String>,
    open: Vec<String>,
    closed: Vec<String>,
    completed: Option<String>,
    in_progress: Option<String>,
    proposed: Option<String>,
}

impl StateCategories {
    fn is_closed(&self, state: &str) -> bool {
        matches!(
            self.by_name.get(state).map(String::as_str),
            Some(COMPLETED | "Removed")
        )
    }

    /// The state to move to when closing, or when reopening.
    fn close_state(&self) -> Option<&str> {
        self.completed.as_deref()
    }

    fn reopen_state(&self) -> Option<&str> {
        self.in_progress.as_deref().or(self.proposed.as_deref())
    }

    fn push_open(&mut self, name: &str) {
        if !self.open.iter().any(|existing| existing == name) {
            self.open.push(name.to_owned());
        }
    }

    fn push_closed(&mut self, name: &str) {
        if !self.closed.iter().any(|existing| existing == name) {
            self.closed.push(name.to_owned());
        }
    }
}

/// Reads the states of both issue work-item types.
async fn state_categories(
    http: &ForgeHttp,
    api: &str,
    repo: &ForgeRepoRef,
) -> Result<StateCategories> {
    let project = project_api(api, repo);
    let mut categories = StateCategories::default();
    for kind in ISSUE_TYPES {
        let url = format!("{project}/_apis/wit/workitemtypes/{kind}/states?api-version=7.1");
        let value = http.get_json(&url).await?;
        for state in value["value"].as_array().into_iter().flatten() {
            let name = text(state, "name");
            let category = text(state, "category");
            if name.is_empty() || category.is_empty() {
                continue;
            }
            categories.by_name.insert(name.clone(), category.clone());
            match category.as_str() {
                COMPLETED | "Removed" => categories.push_closed(&name),
                _ => categories.push_open(&name),
            }
            match category.as_str() {
                COMPLETED if categories.completed.is_none() => {
                    categories.completed = Some(name);
                }
                IN_PROGRESS if categories.in_progress.is_none() => {
                    categories.in_progress = Some(name);
                }
                PROPOSED if categories.proposed.is_none() => {
                    categories.proposed = Some(name);
                }
                _ => {}
            }
        }
    }
    if categories.by_name.is_empty() {
        return Err(ForgeError::Parse(
            "Azure Boards returned no work-item states for Issue or Bug".into(),
        ));
    }
    Ok(categories)
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
        "{}/_apis/wit/workitems?ids={list}&fields=System.Id,System.Title,System.State,System.WorkItemType,System.CreatedBy,System.AssignedTo,System.Tags,System.CommentCount,System.CreatedDate,System.ChangedDate,System.Description,Microsoft.VSTS.Common.ClosedDate,Microsoft.VSTS.Common.StateChangeDate&api-version=7.1",
        project_api(api, repo)
    );
    let value = http.get_json(&url).await?;
    Ok(value["value"].as_array().cloned().unwrap_or_default())
}

fn quoted_list(names: &[String]) -> String {
    names
        .iter()
        .map(|name| format!("'{}'", name.replace('\'', "''")))
        .collect::<Vec<_>>()
        .join(", ")
}

fn query_text(
    filter: ForgeIssueFilter,
    search: Option<&str>,
    categories: &StateCategories,
) -> Result<String> {
    let work_types = ISSUE_TYPES
        .iter()
        .map(|kind| format!("'{kind}'"))
        .collect::<Vec<_>>()
        .join(", ");
    let mut clauses = vec![
        "[System.TeamProject] = @project".to_owned(),
        format!("[System.WorkItemType] IN ({work_types})"),
    ];
    match filter {
        ForgeIssueFilter::Open | ForgeIssueFilter::Assigned | ForgeIssueFilter::Created => {
            if !categories.closed.is_empty() {
                clauses.push(format!(
                    "[System.State] NOT IN ({})",
                    quoted_list(&categories.closed)
                ));
            }
        }
        ForgeIssueFilter::Closed => {
            if !categories.closed.is_empty() {
                clauses.push(format!(
                    "[System.State] IN ({})",
                    quoted_list(&categories.closed)
                ));
            }
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
    let categories = state_categories(http, api, repo).await?;
    let query = query_text(filter, search, &categories)?;
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
            .filter_map(|item| map_summary(repo, item, &categories))
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
    let categories = state_categories(http, api, repo).await?;
    map_issue(repo, &value, &categories)
}

/// Reads one work item's comments.
pub(crate) async fn comments(
    http: &ForgeHttp,
    api: &str,
    repo: &ForgeRepoRef,
    number: u32,
    cursor: Option<&str>,
) -> Result<ForgeIssueCommentPage> {
    validate_repo(repo)?;
    let mut url = format!(
        "{}/_apis/wit/workItems/{number}/comments?api-version=7.1-preview.4",
        project_api(api, repo)
    );
    if let Some(cursor) = cursor.filter(|cursor| !cursor.is_empty()) {
        url.push_str(&format!("&continuationToken={}", encode(cursor)));
    }
    let value = http.get_json(&url).await?;
    let items = value["comments"].as_array().cloned().unwrap_or_default();
    let next_cursor = value["continuationToken"]
        .as_str()
        .filter(|token| !token.is_empty())
        .map(ToOwned::to_owned);
    Ok(ForgeIssueCommentPage {
        repo: repo.clone(),
        number,
        comments: items.iter().filter_map(map_comment).collect(),
        has_more: next_cursor.is_some(),
        next_cursor,
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
        // Scrum and Basic call the finished state `Done`; only the category
        // is portable across processes.
        let categories = state_categories(http, api, repo).await?;
        let target = match state {
            ForgeIssueState::Closed => categories.close_state(),
            ForgeIssueState::Open => categories.reopen_state(),
        }
        .ok_or_else(|| {
            ForgeError::Unsupported(format!(
                "this Azure Boards process has no {COMPLETED} state to {} the work item",
                if state == ForgeIssueState::Closed {
                    "close"
                } else {
                    "reopen"
                }
            ))
        })?;
        patch.push(json!({
            "op": "add",
            "path": "/fields/System.State",
            "value": target,
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

fn map_issue(
    repo: &ForgeRepoRef,
    value: &Value,
    categories: &StateCategories,
) -> Result<ForgeIssue> {
    let summary = map_summary(repo, value, categories)
        .ok_or_else(|| ForgeError::Parse("Azure DevOps returned an incomplete work item".into()))?;
    let closed = summary.state == ForgeIssueState::Closed;
    let linked_pull_requests = value["relations"]
        .as_array()
        .map(|relations| {
            relations
                .iter()
                .filter(|relation| relation["rel"].as_str() == Some("ArtifactLink"))
                .filter_map(|relation| map_linked_pr(repo, relation))
                .collect()
        })
        .unwrap_or_default();
    Ok(ForgeIssue {
        summary,
        body: text(&value["fields"], "System.Description"),
        created_at: parse_datetime(&value["fields"]["System.CreatedDate"], "System.CreatedDate")
            .ok(),
        // The real closure date; the last edit is not when the item closed.
        closed_at: closed
            .then(|| {
                [
                    "Microsoft.VSTS.Common.ClosedDate",
                    "Microsoft.VSTS.Common.StateChangeDate",
                ]
                .iter()
                .find_map(|field| {
                    parse_optional_datetime(value["fields"].get(field), field)
                        .ok()
                        .flatten()
                })
            })
            .flatten(),
        close_reason: None,
        linked_pull_requests,
    })
}

fn map_summary(
    repo: &ForgeRepoRef,
    value: &Value,
    categories: &StateCategories,
) -> Option<ForgeIssueSummary> {
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
        state: if categories.is_closed(&state) {
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
        updated_at: parse_datetime(&fields["System.ChangedDate"], "System.ChangedDate").ok(),
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

/// Decodes the `%2F` separators an Azure artifact link uses.
fn percent_decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut output = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%'
            && index + 2 < bytes.len()
            && let (Some(high), Some(low)) =
                (hex_value(bytes[index + 1]), hex_value(bytes[index + 2]))
        {
            output.push(high * 16 + low);
            index += 3;
            continue;
        }
        output.push(bytes[index]);
        index += 1;
    }
    String::from_utf8_lossy(&output).into_owned()
}

fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

/// Parses a documented `vstfs:///Git/PullRequestId/{project}%2F{repo}%2F{pr}`
/// artifact link. The web URL is built from the repository being read, not
/// trusted from the link.
fn map_linked_pr(repo: &ForgeRepoRef, relation: &Value) -> Option<ForgeIssueLinkedPr> {
    let url = optional_text(relation, "url")?;
    let artifact = url.strip_prefix("vstfs:///Git/PullRequestId/")?;
    let decoded = percent_decode(artifact);
    let mut parts = decoded.split('/');
    let (Some(_project), Some(_repository), Some(number)) =
        (parts.next(), parts.next(), parts.next())
    else {
        return None;
    };
    let number = number.parse::<u32>().ok()?;
    Some(ForgeIssueLinkedPr {
        number,
        title: optional_text(&relation["attributes"], "name")
            .unwrap_or_else(|| format!("Pull request #{number}")),
        state: None,
        web_url: format!(
            "{}/{}/_git/{}/pullrequest/{number}",
            repo.base_url.trim_end_matches('/'),
            encode(&repo.owner),
            encode(&repo.repo)
        ),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scrum() -> StateCategories {
        let mut categories = StateCategories::default();
        for (name, category) in [
            ("New", PROPOSED),
            ("Active", IN_PROGRESS),
            ("Resolved", "Resolved"),
            ("Done", COMPLETED),
            ("Removed", "Removed"),
        ] {
            categories.by_name.insert(name.into(), category.into());
            match category {
                COMPLETED | "Removed" => categories.push_closed(name),
                _ => categories.push_open(name),
            }
            match category {
                COMPLETED if categories.completed.is_none() => {
                    categories.completed = Some(name.into());
                }
                IN_PROGRESS if categories.in_progress.is_none() => {
                    categories.in_progress = Some(name.into());
                }
                PROPOSED if categories.proposed.is_none() => {
                    categories.proposed = Some(name.into());
                }
                _ => {}
            }
        }
        categories
    }

    fn repo() -> ForgeRepoRef {
        ForgeRepoRef {
            forge: h4rn_protocol::ForgeKind::AzureDevOps,
            base_url: "https://dev.azure.com/acme".into(),
            owner: "Platform".into(),
            repo: "widget".into(),
        }
    }

    #[test]
    fn the_state_category_decides_whether_a_work_item_is_closed() {
        for state in ["Done", "Removed"] {
            let value = json!({
                "id": 3,
                "fields": {
                    "System.State": state,
                    "System.ChangedDate": "2026-01-01T00:00:00Z",
                    "System.CreatedDate": "2026-01-01T00:00:00Z"
                }
            });
            assert_eq!(
                map_summary(&repo(), &value, &scrum()).map(|issue| issue.state),
                Some(ForgeIssueState::Closed),
                "{state}"
            );
        }
        // Scrum's in-progress state is `Active`, and it is not closed just
        // because a different process calls its open state `New`.
        let value = json!({
            "id": 3,
            "fields": {"System.State": "Active", "System.ChangedDate": "2026-01-01T00:00:00Z"}
        });
        assert_eq!(
            map_summary(&repo(), &value, &scrum()).map(|issue| issue.state),
            Some(ForgeIssueState::Open)
        );
        assert_eq!(scrum().close_state(), Some("Done"));
        assert_eq!(scrum().reopen_state(), Some("Active"));
    }

    #[test]
    fn the_filters_name_the_processes_own_states() {
        let open = query_text(ForgeIssueFilter::Open, None, &scrum()).unwrap();
        assert!(
            open.contains("[System.State] NOT IN ('Done', 'Removed')"),
            "{open}"
        );
        let closed = query_text(ForgeIssueFilter::Closed, None, &scrum()).unwrap();
        assert!(
            closed.contains("[System.State] IN ('Done', 'Removed')"),
            "{closed}"
        );
    }

    #[test]
    fn a_pull_request_artifact_link_is_decoded_and_url_built_from_the_repo() {
        let relation = json!({
            "rel": "ArtifactLink",
            "url": "vstfs:///Git/PullRequestId/6f1c2e40-aaaa-bbbb-cccc-0123456789ab%2F9d2a1b7c-1111-2222-3333-444455556666%2F42",
            "attributes": {"name": "Pull request 42"}
        });
        let linked = map_linked_pr(&repo(), &relation).expect("linked");
        assert_eq!(linked.number, 42);
        assert_eq!(
            linked.web_url,
            "https://dev.azure.com/acme/Platform/_git/widget/pullrequest/42"
        );
    }

    #[test]
    fn an_http_relation_is_not_a_documented_pr_artifact() {
        let relation = json!({
            "rel": "ArtifactLink",
            "url": "https://dev.azure.com/acme/Platform/_git/widget/pullrequest/9"
        });
        assert!(map_linked_pr(&repo(), &relation).is_none());
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
        let query = query_text(ForgeIssueFilter::Open, Some("it's broken"), &scrum()).unwrap();
        assert!(query.contains("it''s broken"));
    }
}
