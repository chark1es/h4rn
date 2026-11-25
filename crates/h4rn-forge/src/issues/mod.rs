//! Shared issue helpers and the Linear GraphQL client.
//!
//! Forge issue mappings live next to their provider; this module keeps the
//! pieces every provider needs (labels, people, state names) and the Linear
//! client, whose model (teams, workflow states, cursors) is different enough to
//! stand alone. Requests carry the same redaction as forge calls.

use std::time::Duration;

use chrono::{DateTime, Utc};
use h4rn_protocol::issues::{
    ForgeIssueCapabilities, ForgeIssueFilter, ForgeIssuePerson, ForgeIssueState, LinearIssue,
    LinearIssueComment, LinearIssueLinkedPr, LinearIssuePage, LinearIssueSummary, LinearStateType,
    LinearTeam,
};
use h4rn_protocol::{ForgeKind, ForgeLabel};
use serde_json::{Value, json};

use crate::error::{ForgeError, Result};
use crate::http::redact_configured;

pub(crate) mod azure;
pub(crate) mod gitea;
pub(crate) mod github;
pub(crate) mod gitlab;

/// What each forge's issue tracker can do.
pub fn capabilities(kind: ForgeKind) -> ForgeIssueCapabilities {
    match kind {
        ForgeKind::GitHub => ForgeIssueCapabilities {
            read: true,
            write: true,
            comments: true,
            labels: true,
            assignees: true,
            close_reason: true,
            search: true,
            assigned_filter: true,
            created_filter: true,
            mentioning_filter: true,
            linked_pull_requests: true,
        },
        ForgeKind::GitLab => ForgeIssueCapabilities {
            read: true,
            write: true,
            comments: true,
            labels: true,
            assignees: true,
            close_reason: false,
            search: true,
            assigned_filter: true,
            created_filter: true,
            mentioning_filter: false,
            linked_pull_requests: true,
        },
        ForgeKind::Gitea | ForgeKind::Forgejo => ForgeIssueCapabilities {
            read: true,
            write: true,
            comments: true,
            labels: true,
            assignees: true,
            close_reason: false,
            search: true,
            assigned_filter: true,
            created_filter: true,
            mentioning_filter: true,
            linked_pull_requests: false,
        },
        // Atlassian removed Bitbucket Cloud's issue tracker API in August
        // 2026. The tracker is gone, not merely disabled, so the capability
        // row says unavailable rather than advertising a broken surface.
        ForgeKind::Bitbucket => ForgeIssueCapabilities::default(),
        ForgeKind::AzureDevOps => ForgeIssueCapabilities {
            read: true,
            write: true,
            comments: true,
            labels: true,
            assignees: true,
            close_reason: false,
            search: true,
            assigned_filter: true,
            created_filter: true,
            mentioning_filter: false,
            linked_pull_requests: true,
        },
    }
}

/// The refusal for a forge whose issue tracker no longer exists.
pub(crate) fn unavailable(kind: ForgeKind) -> ForgeError {
    ForgeError::Unsupported(format!(
        "{} does not expose an issue tracker: Atlassian removed Bitbucket Cloud's issue API in August 2026",
        kind.display_name()
    ))
}

/// Linear's public GraphQL endpoint.
pub const LINEAR_ENDPOINT: &str = "https://api.linear.app/graphql";
/// Issues returned per page.
pub const LINEAR_PAGE_SIZE: u32 = 50;

/// Identity behind a Linear API key.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LinearViewer {
    /// Workspace or user display name.
    pub name: String,
    /// Account email, when reported.
    pub email: Option<String>,
}

/// A small Linear GraphQL client.
///
/// The API key lives only in this struct and is redacted from every error. The
/// client is rebuilt from the daemon's secret store when needed, never
/// persisted here.
pub struct LinearClient {
    client: reqwest::Client,
    endpoint: String,
    key: String,
}

impl LinearClient {
    /// Builds a client for the public endpoint.
    pub fn new(api_key: &str) -> Result<Self> {
        Self::with_endpoint(api_key, LINEAR_ENDPOINT)
    }

    /// Builds a client for an explicit endpoint, used by wiremock tests.
    pub fn with_endpoint(api_key: &str, endpoint: &str) -> Result<Self> {
        let key = api_key
            .trim()
            .trim_start_matches("Bearer ")
            .trim()
            .to_owned();
        if key.is_empty() {
            return Err(ForgeError::InvalidRequest(
                "the Linear API key is empty".into(),
            ));
        }
        let client = reqwest::Client::builder()
            .user_agent(concat!("h4rn-forge/", env!("CARGO_PKG_VERSION")))
            .timeout(Duration::from_secs(30))
            .build()
            .map_err(|error| ForgeError::Network(redact_configured(&error.to_string(), &key)))?;
        Ok(Self {
            client,
            endpoint: endpoint.to_owned(),
            key,
        })
    }

    async fn query(&self, query: &str, variables: Value) -> Result<Value> {
        let response = self
            .client
            .post(&self.endpoint)
            .header("authorization", &self.key)
            .json(&json!({"query": query, "variables": variables}))
            .send()
            .await
            .map_err(|error| ForgeError::Network(self.sanitize(&error.to_string())))?;
        let status = response.status();
        let body = response
            .text()
            .await
            .map_err(|error| ForgeError::Network(self.sanitize(&error.to_string())))?;
        // A `404` is Linear saying the entity does not exist. Any body it
        // carries, JSON or not, can quote the id that was asked for, so the
        // status is mapped before the body is parsed or forwarded.
        if status.as_u16() == 404 {
            return Err(linear_not_found());
        }
        let value: Value = serde_json::from_str(&body).map_err(|error| {
            ForgeError::Parse(format!(
                "Linear returned a non-JSON body: {}",
                self.sanitize(&error.to_string())
            ))
        })?;
        if !status.is_success() {
            let message = self.sanitize(
                value["message"]
                    .as_str()
                    .or_else(|| value["errors"][0]["message"].as_str())
                    .unwrap_or("Linear rejected the request"),
            );
            // A rejected key, a missing permission or a rate limit is the
            // real answer even when its text mentions something "not found";
            // only a reply that names a missing entity becomes a miss.
            return Err(match status.as_u16() {
                401 => ForgeError::InvalidRequest(format!(
                    "the Linear API key was rejected: {message}"
                )),
                403 => ForgeError::LinearForbidden { message },
                429 => ForgeError::LinearRateLimited { message },
                _ if value["errors"]
                    .as_array()
                    .is_some_and(|errors| errors.iter().any(is_not_found)) =>
                {
                    linear_not_found()
                }
                _ => ForgeError::Http {
                    status: status.as_u16(),
                    message,
                },
            });
        }
        if let Some(errors) = value["errors"].as_array() {
            // Linear also reports a missing entity as a GraphQL error with
            // HTTP 200, either through `extensions.code` or its message. Both
            // shapes can echo the id, so they never reach the caller's text.
            if errors.iter().any(is_not_found) {
                return Err(linear_not_found());
            }
            if let Some(message) = errors.first().and_then(|error| error["message"].as_str()) {
                let message = self.sanitize(message);
                // Linear reports an invalid key as a GraphQL error with HTTP 200.
                return Err(if message.to_ascii_lowercase().contains("authentication") {
                    ForgeError::InvalidRequest(format!(
                        "the Linear API key was rejected: {message}"
                    ))
                } else {
                    ForgeError::InvalidRequest(message)
                });
            }
        }
        Ok(value["data"].clone())
    }

    /// Removes the configured key, in every spelling an upstream body can
    /// quote, before a message leaves this client. The pattern pass stays as
    /// a second layer for a credential this account does not own.
    fn sanitize(&self, value: &str) -> String {
        redact_configured(value, &self.key)
    }

    /// Validates the key and returns the workspace identity.
    pub async fn viewer(&self) -> Result<LinearViewer> {
        let data = self.query(LINEAR_VIEWER_QUERY, json!({})).await?;
        let name = text(&data["viewer"]["organization"], "name");
        let name = if name.is_empty() {
            text(&data["viewer"], "name")
        } else {
            name
        };
        Ok(LinearViewer {
            name,
            email: optional_text(&data["viewer"], "email"),
        })
    }

    /// Teams visible to the key.
    pub async fn teams(&self) -> Result<Vec<LinearTeam>> {
        let data = self.query(LINEAR_TEAMS_QUERY, json!({})).await?;
        Ok(data["teams"]["nodes"]
            .as_array()
            .map(|teams| {
                teams
                    .iter()
                    .map(|team| LinearTeam {
                        id: text(team, "id"),
                        key: text(team, "key"),
                        name: text(team, "name"),
                    })
                    .collect()
            })
            .unwrap_or_default())
    }

    /// Lists one team's issues. `cursor` is the previous page's `endCursor`.
    pub async fn list_issues(
        &self,
        team_id: &str,
        filter: ForgeIssueFilter,
        cursor: Option<&str>,
        query: Option<&str>,
    ) -> Result<LinearIssuePage> {
        let involvement = match filter {
            ForgeIssueFilter::Assigned => Some(json!({"assignee": {"isMe": {"eq": true}}})),
            ForgeIssueFilter::Created => Some(json!({"creator": {"isMe": {"eq": true}}})),
            ForgeIssueFilter::Mentioning => {
                return Err(ForgeError::Unsupported(
                    "Linear has no issue filter for mentions; search instead".into(),
                ));
            }
            _ => None,
        };
        let state = match filter {
            ForgeIssueFilter::Closed => {
                Some(json!({"state": {"type": {"in": ["completed", "canceled", "duplicate"]}}}))
            }
            ForgeIssueFilter::Open | ForgeIssueFilter::Assigned | ForgeIssueFilter::Created => {
                Some(
                    json!({"state": {"type": {"in": ["triage", "backlog", "unstarted", "started"]}}}),
                )
            }
            ForgeIssueFilter::All | ForgeIssueFilter::Mentioning => None,
        };
        // The team clause lives in the document (`$teamId`), so both list and
        // search carry exactly this filter: the same state and involvement
        // conditions, whichever endpoint answers.
        let mut filter_value = json!({});
        if let Some(state) = state {
            merge(&mut filter_value, state);
        }
        if let Some(involvement) = involvement {
            merge(&mut filter_value, involvement);
        }

        let term = query.map(str::trim).filter(|term| !term.is_empty());
        let (document, variables) = match term {
            Some(term) => (
                LINEAR_SEARCH_QUERY,
                json!({
                    "teamId": team_id,
                    "term": term,
                    "filter": filter_value,
                    "first": LINEAR_PAGE_SIZE,
                    "after": cursor,
                }),
            ),
            None => (
                LINEAR_LIST_QUERY,
                json!({
                    "teamId": team_id,
                    "filter": filter_value,
                    "first": LINEAR_PAGE_SIZE,
                    "after": cursor,
                }),
            ),
        };
        let data = self.query(document, variables).await?;
        let connection = if term.is_some() {
            &data["searchIssues"]
        } else {
            &data["issues"]
        };
        let issues = connection["nodes"]
            .as_array()
            .map(|nodes| nodes.iter().filter_map(map_issue_summary).collect())
            .unwrap_or_default();
        let next_cursor = connection["pageInfo"]["hasNextPage"]
            .as_bool()
            .unwrap_or(false)
            .then(|| optional_text(&connection["pageInfo"], "endCursor"))
            .flatten();
        Ok(LinearIssuePage {
            issues,
            next_cursor,
        })
    }

    /// Reads one issue and its comments.
    ///
    /// `comments_after` is Linear's comment cursor; page one omits it.
    pub async fn get_issue(
        &self,
        issue_id: &str,
        comments_after: Option<&str>,
    ) -> Result<LinearIssue> {
        let data = self
            .query(
                LINEAR_DETAIL_QUERY,
                json!({"id": issue_id, "after": comments_after}),
            )
            .await?;
        let issue = &data["issue"];
        // The id is Linear's internal UUID, which nobody recognises. The
        // caller that knows the human identifier names the issue itself.
        let summary = map_issue_summary(issue).ok_or_else(linear_not_found)?;
        let comments = issue["comments"]["nodes"]
            .as_array()
            .map(|nodes| nodes.iter().filter_map(map_comment).collect())
            .unwrap_or_default();
        let linked_pull_requests = issue["attachments"]["nodes"]
            .as_array()
            .map(|nodes| nodes.iter().filter_map(map_attachment).collect())
            .unwrap_or_default();
        let comments_next_cursor = issue["comments"]["pageInfo"]["hasNextPage"]
            .as_bool()
            .unwrap_or(false)
            .then(|| optional_text(&issue["comments"]["pageInfo"], "endCursor"))
            .flatten();
        Ok(LinearIssue {
            summary,
            description: text(issue, "description"),
            created_at: parse_datetime(&issue["createdAt"], "createdAt").ok(),
            comments,
            comments_next_cursor,
            linked_pull_requests,
        })
    }

    /// Reads only the issue's team id.
    ///
    /// The ownership check needs nothing else, so it asks Linear for one
    /// field instead of the whole discussion.
    pub async fn team_id(&self, issue_id: &str) -> Result<String> {
        let data = self
            .query(LINEAR_ISSUE_TEAM_QUERY, json!({ "id": issue_id }))
            .await?;
        optional_text(&data["issue"]["team"], "id").ok_or_else(linear_not_found)
    }

    /// Posts a comment and returns what Linear stored.
    pub async fn create_comment(&self, issue_id: &str, body: &str) -> Result<LinearIssueComment> {
        let data = self
            .query(
                LINEAR_COMMENT_MUTATION,
                json!({"issueId": issue_id, "body": body}),
            )
            .await?;
        let comment = &data["commentCreate"]["comment"];
        map_comment(comment)
            .ok_or_else(|| ForgeError::Parse("Linear returned no comment after posting".into()))
    }
}

/// Linear's "not found" reply, whichever shape it takes.
///
/// The text is the daemon's own: Linear often echoes the id it was asked with,
/// and for a linked issue that is an internal UUID nobody recognises.
fn linear_not_found() -> ForgeError {
    ForgeError::NotFound("Linear issue was not found".into())
}

/// Whether one GraphQL error reports a missing entity.
///
/// Only Linear's `NOT_FOUND` code or its "Entity not found" wording counts.
/// Other errors can also say "not found" ("API key not found"), and those
/// must keep their own meaning.
fn is_not_found(error: &Value) -> bool {
    match error["extensions"]["code"].as_str() {
        Some("NOT_FOUND") => return true,
        Some(_) => return false,
        None => {}
    }
    error["message"]
        .as_str()
        .is_some_and(|message| message.to_ascii_lowercase().contains("entity not found"))
}

/// GraphQL document that validates a key and reads the workspace identity.
pub const LINEAR_VIEWER_QUERY: &str = r#"
query {
  viewer { name email organization { name } }
}"#;

/// GraphQL document that lists the teams a key can see.
pub const LINEAR_TEAMS_QUERY: &str = r#"
query {
  teams(first: 100) { nodes { id key name } }
}"#;

/// GraphQL document that lists a team's issues.
///
/// `$teamId` is used inside the filter, and `$filter` carries the state and
/// involvement conditions the search also sends.
pub const LINEAR_LIST_QUERY: &str = r#"
query($teamId: ID!, $filter: IssueFilter!, $first: Int!, $after: String) {
  issues(
    first: $first
    after: $after
    orderBy: updatedAt
    filter: { and: [{ team: { id: { eq: $teamId } } }, $filter] }
  ) {
    nodes {
      id identifier title updatedAt url
      state { name type }
      team { id key }
      assignee { displayName }
      labels { nodes { name } }
    }
    pageInfo { hasNextPage endCursor }
  }
}"#;

/// GraphQL document that searches a team's issues with the list's filter.
///
/// `searchIssues` takes the team as its own `teamId` argument and types it as
/// `String`, unlike `issues`, whose team filter compares `ID`s. The team is
/// therefore not repeated inside `$filter`.
pub const LINEAR_SEARCH_QUERY: &str = r#"
query($teamId: String, $term: String!, $filter: IssueFilter, $first: Int!, $after: String) {
  searchIssues(
    term: $term
    teamId: $teamId
    first: $first
    after: $after
    filter: $filter
  ) {
    nodes {
      id identifier title updatedAt url
      state { name type }
      team { id key }
      assignee { displayName }
      labels { nodes { name } }
    }
    pageInfo { hasNextPage endCursor }
  }
}"#;

/// GraphQL document that reads one issue and its comments.
pub const LINEAR_DETAIL_QUERY: &str = r#"
query($id: String!, $after: String) {
  issue(id: $id) {
    id identifier title description createdAt updatedAt url
    state { name type }
    team { id key }
    assignee { displayName }
    labels { nodes { name } }
    comments(first: 100, after: $after) {
      nodes { id body createdAt updatedAt user { displayName } }
      pageInfo { hasNextPage endCursor }
    }
    attachments(first: 100) { nodes { title url metadata } }
  }
}"#;

/// GraphQL document that reads only an issue's team id.
pub const LINEAR_ISSUE_TEAM_QUERY: &str = r#"
query($id: String!) {
  issue(id: $id) {
    team { id }
  }
}"#;

/// GraphQL mutation that posts one comment.
pub const LINEAR_COMMENT_MUTATION: &str = r#"
mutation($issueId: String!, $body: String!) {
  commentCreate(input: { issueId: $issueId, body: $body }) {
    success
    comment { id body createdAt updatedAt user { displayName } }
  }
}"#;

fn map_issue_summary(value: &Value) -> Option<LinearIssueSummary> {
    Some(LinearIssueSummary {
        id: optional_text(value, "id")?,
        identifier: text(value, "identifier"),
        title: text(value, "title"),
        state_name: text(&value["state"], "name"),
        state_type: state_type(&text(&value["state"], "type")),
        team_id: text(&value["team"], "id"),
        team_key: text(&value["team"], "key"),
        // Linear has one assignee per issue; the protocol keeps a list so
        // every tracker shares a row shape.
        assignees: assignee_names(&value["assignee"]),
        labels: string_nodes_named(&value["labels"]),
        updated_at: parse_datetime(&value["updatedAt"], "updatedAt").ok(),
        url: text(value, "url"),
    })
}

fn map_comment(value: &Value) -> Option<LinearIssueComment> {
    Some(LinearIssueComment {
        id: optional_text(value, "id")?,
        author: text(&value["user"], "displayName"),
        body: text(value, "body"),
        created_at: parse_datetime(&value["createdAt"], "createdAt").ok()?,
        updated_at: parse_optional_datetime(value.get("updatedAt")),
    })
}

fn map_attachment(value: &Value) -> Option<LinearIssueLinkedPr> {
    let url = optional_text(value, "url")?;
    if !(url.contains("/pull/")
        || url.contains("/merge_requests/")
        || url.contains("/pull-requests/"))
    {
        return None;
    }
    Some(LinearIssueLinkedPr {
        title: text(value, "title"),
        url,
        state: text(&value["metadata"], "state"),
    })
}

/// Linear returns a single `assignee`, not a connection of them.
fn assignee_names(value: &Value) -> Vec<String> {
    let name = text(value, "displayName");
    if name.is_empty() {
        Vec::new()
    } else {
        vec![name]
    }
}

fn string_nodes_named(value: &Value) -> Vec<String> {
    value["nodes"]
        .as_array()
        .map(|nodes| nodes.iter().map(|node| text(node, "name")).collect())
        .unwrap_or_default()
}

fn state_type(value: &str) -> LinearStateType {
    match value {
        "backlog" => LinearStateType::Backlog,
        "unstarted" => LinearStateType::Unstarted,
        "started" => LinearStateType::Started,
        "completed" => LinearStateType::Completed,
        "canceled" => LinearStateType::Canceled,
        "duplicate" => LinearStateType::Duplicate,
        _ => LinearStateType::Unstarted,
    }
}

fn merge(target: &mut Value, source: Value) {
    if let (Some(target), Value::Object(source)) = (target.as_object_mut(), source) {
        for (key, value) in source {
            target.insert(key, value);
        }
    }
}

fn text(value: &Value, key: &str) -> String {
    value
        .get(key)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned()
}

fn optional_text(value: &Value, key: &str) -> Option<String> {
    value
        .get(key)
        .and_then(Value::as_str)
        .map(ToOwned::to_owned)
}

fn parse_datetime(value: &Value, field: &str) -> Result<DateTime<Utc>> {
    let text = value
        .as_str()
        .ok_or_else(|| ForgeError::Parse(format!("Linear {field} is not a string")))?;
    DateTime::parse_from_rfc3339(text)
        .map(|value| value.with_timezone(&Utc))
        .map_err(|error| ForgeError::Parse(format!("Linear {field} is not RFC 3339: {error}")))
}

fn parse_optional_datetime(value: Option<&Value>) -> Option<DateTime<Utc>> {
    value.and_then(|value| parse_datetime(value, "timestamp").ok())
}

/// Maps a GitHub/Gitea-style label array.
pub(crate) fn labels(value: &Value) -> Vec<ForgeLabel> {
    value
        .as_array()
        .map(|labels| {
            labels
                .iter()
                .map(|label| ForgeLabel {
                    name: text(label, "name"),
                    color: optional_text(label, "color").filter(|color| !color.is_empty()),
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Maps a provider's assignee array (`login`/`name` or GitLab's `username`).
pub(crate) fn people(value: &Value) -> Vec<ForgeIssuePerson> {
    value
        .as_array()
        .map(|people| {
            people
                .iter()
                .filter_map(|person| {
                    let login = optional_text(person, "login")
                        .or_else(|| optional_text(person, "username"))
                        .or_else(|| optional_text(person, "displayName"))?;
                    Some(ForgeIssuePerson {
                        login,
                        name: optional_text(person, "name")
                            .or_else(|| optional_text(person, "display_name"))
                            .or_else(|| optional_text(person, "displayName")),
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Maps an open/closed boolean to the protocol state.
pub(crate) fn state_from_closed(closed: bool) -> ForgeIssueState {
    if closed {
        ForgeIssueState::Closed
    } else {
        ForgeIssueState::Open
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn issue_state_names_map_to_linear_families() {
        assert_eq!(state_type("started"), LinearStateType::Started);
        assert_eq!(state_type("completed"), LinearStateType::Completed);
        assert_eq!(state_type("something"), LinearStateType::Unstarted);
    }

    #[tokio::test]
    async fn a_missing_linear_issue_is_not_named_by_its_internal_id() {
        use wiremock::matchers::method;
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": {"issue": null}
            })))
            .mount(&server)
            .await;
        let client = LinearClient::with_endpoint("test-key", &server.uri()).unwrap();
        let error = client.get_issue("issue_uuid_1", None).await.unwrap_err();
        let message = error.to_string();
        assert!(message.contains("Linear issue was not found"));
        assert!(
            !message.contains("issue_uuid_1"),
            "the provider UUID must never reach the person: {message}"
        );
    }

    /// Reads an issue through one canned reply and returns the error.
    async fn missing_issue_reply(status: u16, body: Value) -> ForgeError {
        use wiremock::matchers::method;
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(status).set_body_json(body))
            .mount(&server)
            .await;
        let client = LinearClient::with_endpoint("test-key", &server.uri()).unwrap();
        client.get_issue("issue_uuid_1", None).await.unwrap_err()
    }

    #[tokio::test]
    async fn every_linear_not_found_shape_hides_the_internal_id() {
        // Each reply shape Linear uses for a missing entity, with the id it
        // was asked for echoed back the way the real API does.
        let shapes = [
            (
                "http 404",
                404,
                json!({"message": "Entity issue_uuid_1 was not found"}),
            ),
            (
                "graphql not-found code",
                200,
                json!({"errors": [{
                    "message": "Nothing matches issue_uuid_1",
                    "extensions": {"code": "NOT_FOUND"}
                }]}),
            ),
            (
                "graphql entity-not-found message",
                200,
                json!({"errors": [{"message": "Entity not found: issue_uuid_1"}]}),
            ),
        ];
        for (shape, status, body) in shapes {
            let error = missing_issue_reply(status, body).await;
            assert!(
                matches!(error, ForgeError::NotFound(_)),
                "{shape} must be a not-found reply: {error}"
            );
            let message = error.to_string();
            assert!(
                message.contains("Linear issue was not found"),
                "{shape}: {message}"
            );
            assert!(
                !message.contains("issue_uuid_1"),
                "{shape} must not forward the id: {message}"
            );
        }
    }

    #[tokio::test]
    async fn a_plain_text_404_is_still_a_named_miss() {
        use wiremock::matchers::method;
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(
                ResponseTemplate::new(404).set_body_string("<html>issue_uuid_1 not here</html>"),
            )
            .mount(&server)
            .await;
        let client = LinearClient::with_endpoint("test-key", &server.uri()).unwrap();
        let error = client.get_issue("issue_uuid_1", None).await.unwrap_err();
        assert!(
            matches!(error, ForgeError::NotFound(_)),
            "a non-JSON 404 must be a miss, not a parse error: {error}"
        );
        assert!(!error.to_string().contains("issue_uuid_1"));
    }

    #[tokio::test]
    async fn a_missing_key_is_not_a_missing_issue() {
        // Each of these says "not found" about the key, not the issue, so the
        // person must be told to reconnect rather than that the issue is gone.
        let shapes = [
            (
                "http 401",
                401,
                json!({"errors": [{"message": "Authentication token not found"}]}),
            ),
            (
                "graphql unauthenticated code",
                200,
                json!({"errors": [{
                    "message": "API key not found",
                    "extensions": {"code": "UNAUTHENTICATED"}
                }]}),
            ),
            (
                "http 400 unauthenticated code",
                400,
                json!({"errors": [{
                    "message": "API key not found",
                    "extensions": {"code": "UNAUTHENTICATED"}
                }]}),
            ),
        ];
        for (shape, status, body) in shapes {
            let error = missing_issue_reply(status, body).await;
            assert!(
                !matches!(error, ForgeError::NotFound(_)),
                "{shape} is a key problem, not a missing issue: {error}"
            );
        }
    }

    #[test]
    fn only_pull_request_attachments_are_linked() {
        let pull = json!({"title": "Fix", "url": "https://github.com/o/r/pull/4"});
        let doc = json!({"title": "Doc", "url": "https://example.test/doc"});
        assert_eq!(map_attachment(&pull).map(|pr| pr.title), Some("Fix".into()));
        assert!(map_attachment(&doc).is_none());
    }
}
