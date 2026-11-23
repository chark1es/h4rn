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

impl StateCategories {
    fn from_states(value: &Value) -> Self {
        let mut categories = Self::default();
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
        categories
    }
}

/// Every issue work-item type the project defines, with its own states.
///
/// Azure processes do not share a type catalogue: Basic has no `Bug`, Scrum
/// has no `Issue`, and each type can name its finished state differently.
/// States are therefore kept per type, and an operation on one work item uses
/// that item's own type.
#[derive(Clone, Debug, Default)]
struct TypeStates {
    by_type: HashMap<String, StateCategories>,
    order: Vec<String>,
}

impl TypeStates {
    fn is_closed(&self, kind: &str, state: &str) -> bool {
        match self.by_type.get(kind) {
            Some(categories) => categories.is_closed(state),
            // A work item type the project has since changed: fall back to
            // every known type rather than reporting a closed item as open.
            None => self
                .by_type
                .values()
                .any(|categories| categories.is_closed(state)),
        }
    }

    fn closed_states(&self, kind: &str) -> &[String] {
        self.by_type
            .get(kind)
            .map(|categories| categories.closed.as_slice())
            .unwrap_or_default()
    }
}

fn is_not_found(error: &ForgeError) -> bool {
    match error {
        ForgeError::NotFound(_) => true,
        ForgeError::Http { status, .. } => *status == 404,
        _ => false,
    }
}

/// Reads one work-item type's states, or `None` when the process has no such
/// type (Azure answers 404).
async fn state_categories_for(
    http: &ForgeHttp,
    project: &str,
    kind: &str,
) -> Result<Option<StateCategories>> {
    let url = format!("{project}/_apis/wit/workitemtypes/{kind}/states?api-version=7.1");
    match http.get_json(&url).await {
        Ok(value) => {
            let categories = StateCategories::from_states(&value);
            Ok((!categories.by_name.is_empty()).then_some(categories))
        }
        Err(error) if is_not_found(&error) => Ok(None),
        Err(error) => Err(error),
    }
}

/// True when a work-item type's reference name is one Azure gives an `Issue`
/// or `Bug`: the system `Microsoft.VSTS.WorkItemTypes.<type>` form, or the
/// inherited-process `<Process>.<type>` form. `Microsoft.<type>` is not one of
/// Azure's names.
fn is_issue_reference(reference: &str) -> bool {
    const SYSTEM: &str = "Microsoft.VSTS.WorkItemTypes.";
    if let Some(kind) = reference.strip_prefix(SYSTEM) {
        return ISSUE_TYPES.contains(&kind);
    }
    match reference.rsplit_once('.') {
        Some((process, kind)) => {
            !process.is_empty()
                && !process.eq_ignore_ascii_case("Microsoft")
                && ISSUE_TYPES.contains(&kind)
        }
        None => false,
    }
}

/// Discovers the project's issue work-item types and reads each one's states.
///
/// A type whose states endpoint 404s is not defined by this process and is
/// skipped; at least one type must remain.
async fn type_states(http: &ForgeHttp, api: &str, repo: &ForgeRepoRef) -> Result<TypeStates> {
    let project = project_api(api, repo);
    let available = http
        .get_json(&format!(
            "{project}/_apis/wit/workitemtypes?api-version=7.1"
        ))
        .await?;
    let mut order: Vec<String> = Vec::new();
    for kind in available["value"].as_array().into_iter().flatten() {
        let name = text(kind, "name");
        let reference = text(kind, "referenceName");
        let documented =
            ISSUE_TYPES.iter().any(|known| *known == name) || is_issue_reference(&reference);
        if name.is_empty() || !documented || order.contains(&name) {
            continue;
        }
        order.push(name);
    }
    if order.is_empty() {
        return Err(ForgeError::Parse(
            "this Azure Boards process has no Issue or Bug work-item type".into(),
        ));
    }
    let mut by_type = HashMap::new();
    for kind in &order {
        if let Some(categories) = state_categories_for(http, &project, kind).await? {
            by_type.insert(kind.clone(), categories);
        }
    }
    order.retain(|kind| by_type.contains_key(kind));
    if by_type.is_empty() {
        return Err(ForgeError::Parse(
            "Azure Boards returned no states for this project's Issue or Bug work-item type".into(),
        ));
    }
    Ok(TypeStates { by_type, order })
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

/// Builds the WIQL, filtering each work-item type by its own state names.
///
/// `[System.State] IN ('Done')` is wrong for an `Issue` in a process whose
/// finished state is `Closed`, so the state clause is scoped to the type.
fn query_text(
    filter: ForgeIssueFilter,
    search: Option<&str>,
    states: &TypeStates,
) -> Result<String> {
    let mut clauses = vec!["[System.TeamProject] = @project".to_owned()];
    let mut type_clauses = Vec::new();
    for kind in &states.order {
        let closed = states.closed_states(kind);
        let type_clause = match filter {
            ForgeIssueFilter::Open | ForgeIssueFilter::Assigned | ForgeIssueFilter::Created => {
                if closed.is_empty() {
                    format!("[System.WorkItemType] = '{kind}'")
                } else {
                    format!(
                        "([System.WorkItemType] = '{kind}' AND [System.State] NOT IN ({}))",
                        quoted_list(closed)
                    )
                }
            }
            ForgeIssueFilter::Closed => {
                if closed.is_empty() {
                    continue;
                }
                format!(
                    "([System.WorkItemType] = '{kind}' AND [System.State] IN ({}))",
                    quoted_list(closed)
                )
            }
            ForgeIssueFilter::All => format!("[System.WorkItemType] = '{kind}'"),
            ForgeIssueFilter::Mentioning => {
                return Err(ForgeError::Unsupported(
                    "Azure Boards has no issue filter for mentions; use search".into(),
                ));
            }
        };
        type_clauses.push(type_clause);
    }
    // No type can match a closed prompt because no known state closes one.
    let type_clause = if type_clauses.is_empty() {
        if filter == ForgeIssueFilter::Closed {
            "[System.Id] = 0".to_owned()
        } else {
            format!(
                "[System.WorkItemType] IN ({})",
                states
                    .order
                    .iter()
                    .map(|kind| format!("'{kind}'"))
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        }
    } else {
        type_clauses.join(" OR ")
    };
    clauses.push(format!("({type_clause})"));
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
    let states = type_states(http, api, repo).await?;
    let query = query_text(filter, search, &states)?;
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
            .filter_map(|item| map_summary(repo, item, &states))
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
    let project = project_api(api, repo);
    let url = format!("{project}/_apis/wit/workitems/{number}?$expand=relations&api-version=7.1");
    let value = http.get_json(&url).await?;
    let kind = text(&value["fields"], "System.WorkItemType");
    let categories = state_categories_for(http, &project, &kind)
        .await?
        .ok_or_else(|| {
            ForgeError::Parse(format!(
                "Azure Boards has no work-item type {kind} to read the item's states"
            ))
        })?;
    let states = TypeStates {
        by_type: HashMap::from([(kind.clone(), categories)]),
        order: vec![kind],
    };
    map_issue(http, api, repo, &value, &states).await
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
        // The item's own type decides which state names exist: Scrum and
        // Basic call the finished state `Done`, Agile calls it `Closed`, and
        // a project may define either type.
        let project = project_api(api, repo);
        let current = http
            .get_json(&format!(
                "{project}/_apis/wit/workitems/{number}?api-version=7.1"
            ))
            .await?;
        let kind = text(&current["fields"], "System.WorkItemType");
        let categories = state_categories_for(http, &project, &kind)
            .await?
            .ok_or_else(|| {
                ForgeError::Parse(format!(
                    "Azure Boards has no work-item type {kind} to read the item's states"
                ))
            })?;
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

async fn map_issue(
    http: &ForgeHttp,
    api: &str,
    repo: &ForgeRepoRef,
    value: &Value,
    states: &TypeStates,
) -> Result<ForgeIssue> {
    let summary = map_summary(repo, value, states)
        .ok_or_else(|| ForgeError::Parse("Azure DevOps returned an incomplete work item".into()))?;
    let closed = summary.state == ForgeIssueState::Closed;
    let mut linked_pull_requests = Vec::new();
    for relation in value["relations"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|relation| relation["rel"].as_str() == Some("ArtifactLink"))
    {
        if let Some(linked) = map_linked_pr(http, api, relation).await {
            linked_pull_requests.push(linked);
        }
    }
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
    states: &TypeStates,
) -> Option<ForgeIssueSummary> {
    let fields = &value["fields"];
    let state = text(fields, "System.State");
    let kind = text(fields, "System.WorkItemType");
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
        state: if states.is_closed(&kind, &state) {
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

/// The browser URL of a repository an artifact link names.
///
/// The link carries the team project id and repository id, and the pull
/// request may live in a different repository than the work item. Azure
/// resolves the ids to the repository's own `webUrl`; a failure falls back to
/// the ids, which Azure also accepts in a browser URL.
async fn repository_web_url(
    http: &ForgeHttp,
    api: &str,
    project: &str,
    repository: &str,
) -> Option<String> {
    let url = format!(
        "{}/{}/_apis/git/repositories/{}?api-version=7.1",
        api.trim_end_matches('/'),
        encode(project),
        encode(repository)
    );
    let value = http.get_json(&url).await.ok()?;
    optional_text(&value, "webUrl").filter(|url| !url.is_empty())
}

/// Parses a documented `vstfs:///Git/PullRequestId/{project}%2F{repo}%2F{pr}`
/// artifact link and points it at that repository's web URL, which may differ
/// from the repository checked out.
async fn map_linked_pr(
    http: &ForgeHttp,
    api: &str,
    relation: &Value,
) -> Option<ForgeIssueLinkedPr> {
    let url = optional_text(relation, "url")?;
    let artifact = url.strip_prefix("vstfs:///Git/PullRequestId/")?;
    let decoded = percent_decode(artifact);
    let mut parts = decoded.split('/');
    let (Some(project), Some(repository), Some(number)) =
        (parts.next(), parts.next(), parts.next())
    else {
        return None;
    };
    let number = number.parse::<u32>().ok()?;
    let web_url = match repository_web_url(http, api, project, repository).await {
        Some(web_url) => format!("{}/pullrequest/{number}", web_url.trim_end_matches('/')),
        None => format!(
            "{}/{}/_git/{}/pullrequest/{number}",
            api.trim_end_matches('/'),
            encode(project),
            encode(repository)
        ),
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

    fn categories(states: &[(&str, &str)]) -> StateCategories {
        let mut categories = StateCategories::default();
        for (name, category) in states {
            categories
                .by_name
                .insert((*name).into(), (*category).into());
            match *category {
                COMPLETED | "Removed" => categories.push_closed(name),
                _ => categories.push_open(name),
            }
            match *category {
                COMPLETED if categories.completed.is_none() => {
                    categories.completed = Some((*name).into());
                }
                IN_PROGRESS if categories.in_progress.is_none() => {
                    categories.in_progress = Some((*name).into());
                }
                PROPOSED if categories.proposed.is_none() => {
                    categories.proposed = Some((*name).into());
                }
                _ => {}
            }
        }
        categories
    }

    fn scrum() -> StateCategories {
        categories(&[
            ("New", PROPOSED),
            ("Active", IN_PROGRESS),
            ("Resolved", "Resolved"),
            ("Done", COMPLETED),
            ("Removed", "Removed"),
        ])
    }

    fn bug_types() -> TypeStates {
        TypeStates {
            by_type: HashMap::from([("Bug".to_owned(), scrum())]),
            order: vec!["Bug".to_owned()],
        }
    }

    /// Basic has only an Issue type; Scrum has only a Bug. Agile names the
    /// finished state Closed, while Scrum's Bug calls it Done.
    fn two_types() -> TypeStates {
        TypeStates {
            by_type: HashMap::from([
                (
                    "Issue".to_owned(),
                    categories(&[
                        ("To Do", PROPOSED),
                        ("Doing", IN_PROGRESS),
                        ("Closed", COMPLETED),
                    ]),
                ),
                ("Bug".to_owned(), scrum()),
            ]),
            order: vec!["Issue".to_owned(), "Bug".to_owned()],
        }
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
                    "System.WorkItemType": "Bug",
                    "System.ChangedDate": "2026-01-01T00:00:00Z",
                    "System.CreatedDate": "2026-01-01T00:00:00Z"
                }
            });
            assert_eq!(
                map_summary(&repo(), &value, &bug_types()).map(|issue| issue.state),
                Some(ForgeIssueState::Closed),
                "{state}"
            );
        }
        // Scrum's in-progress state is `Active`, and it is not closed just
        // because a different process calls its open state `New`.
        let value = json!({
            "id": 3,
            "fields": {
                "System.State": "Active",
                "System.WorkItemType": "Bug",
                "System.ChangedDate": "2026-01-01T00:00:00Z"
            }
        });
        assert_eq!(
            map_summary(&repo(), &value, &bug_types()).map(|issue| issue.state),
            Some(ForgeIssueState::Open)
        );
        assert_eq!(bug_types().by_type["Bug"].close_state(), Some("Done"));
        assert_eq!(bug_types().by_type["Bug"].reopen_state(), Some("Active"));
    }

    #[test]
    fn each_type_is_closed_by_its_own_finished_state() {
        let states = two_types();
        for (kind, state, closed) in [
            ("Issue", "Closed", true),
            ("Issue", "Done", false),
            ("Bug", "Done", true),
            ("Bug", "Closed", false),
        ] {
            assert_eq!(states.is_closed(kind, state), closed, "{kind} {state}");
        }
        assert_eq!(states.by_type["Issue"].close_state(), Some("Closed"));
        assert_eq!(states.by_type["Bug"].close_state(), Some("Done"));
    }

    #[test]
    fn the_filters_name_each_types_own_states() {
        let open = query_text(ForgeIssueFilter::Open, None, &bug_types()).unwrap();
        assert!(
            open.contains(
                "[System.WorkItemType] = 'Bug' AND [System.State] NOT IN ('Done', 'Removed')"
            ),
            "{open}"
        );
        let closed = query_text(ForgeIssueFilter::Closed, None, &two_types()).unwrap();
        assert!(
            closed.contains("[System.WorkItemType] = 'Issue' AND [System.State] IN ('Closed')"),
            "{closed}"
        );
        assert!(
            closed.contains(
                "[System.WorkItemType] = 'Bug' AND [System.State] IN ('Done', 'Removed')"
            ),
            "{closed}"
        );
    }

    fn azure_http(server: &wiremock::MockServer) -> ForgeHttp {
        ForgeHttp::new(
            h4rn_protocol::ForgeKind::AzureDevOps,
            &server.uri(),
            None,
            "application/json",
            600,
        )
        .expect("http")
    }

    #[tokio::test]
    async fn a_pull_request_artifact_link_resolves_a_different_repository() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        let other_repo = format!("{}/other/_git/other", server.uri());
        Mock::given(method("GET"))
            .and(path(
                "/acme/6f1c2e40-aaaa-bbbb-cccc-0123456789ab/_apis/git/repositories/9d2a1b7c-1111-2222-3333-444455556666",
            ))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(json!({"webUrl": other_repo})),
            )
            .mount(&server)
            .await;
        let relation = json!({
            "rel": "ArtifactLink",
            "url": "vstfs:///Git/PullRequestId/6f1c2e40-aaaa-bbbb-cccc-0123456789ab%2F9d2a1b7c-1111-2222-3333-444455556666%2F42",
            "attributes": {"name": "Pull request 42"}
        });
        let linked = map_linked_pr(
            &azure_http(&server),
            &format!("{}/acme", server.uri()),
            &relation,
        )
        .await
        .expect("linked");
        assert_eq!(linked.number, 42);
        assert_eq!(
            linked.web_url,
            format!("{other_repo}/pullrequest/42"),
            "the link points at the artifact's repository, not the checkout"
        );
    }

    #[tokio::test]
    async fn a_pull_request_artifact_link_falls_back_to_its_repository_ids() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path(
                "/acme/6f1c2e40-aaaa-bbbb-cccc-0123456789ab/_apis/git/repositories/9d2a1b7c-1111-2222-3333-444455556666",
            ))
            .respond_with(ResponseTemplate::new(404).set_body_json(json!({
                "message": "not found"
            })))
            .mount(&server)
            .await;
        let relation = json!({
            "rel": "ArtifactLink",
            "url": "vstfs:///Git/PullRequestId/6f1c2e40-aaaa-bbbb-cccc-0123456789ab%2F9d2a1b7c-1111-2222-3333-444455556666%2F42"
        });
        let linked = map_linked_pr(
            &azure_http(&server),
            &format!("{}/acme", server.uri()),
            &relation,
        )
        .await
        .expect("linked");
        assert_eq!(
            linked.web_url,
            format!(
                "{}/acme/6f1c2e40-aaaa-bbbb-cccc-0123456789ab/_git/9d2a1b7c-1111-2222-3333-444455556666/pullrequest/42",
                server.uri()
            )
        );
    }

    #[tokio::test]
    async fn an_http_relation_is_not_a_documented_pr_artifact() {
        use wiremock::MockServer;

        let server = MockServer::start().await;
        let relation = json!({
            "rel": "ArtifactLink",
            "url": "https://dev.azure.com/acme/Platform/_git/widget/pullrequest/9"
        });
        assert!(
            map_linked_pr(
                &azure_http(&server),
                &format!("{}/acme", server.uri()),
                &relation
            )
            .await
            .is_none()
        );
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
        let query = query_text(ForgeIssueFilter::Open, Some("it's broken"), &bug_types()).unwrap();
        assert!(query.contains("it''s broken"));
    }
}
