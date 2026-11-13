//! Shared helpers for the forge clients.
//!
//! These functions keep provider modules free of duplicated date parsing,
//! repository validation and check aggregation.

use chrono::{DateTime, Utc};
use h4rn_protocol::{ForgeCheckStatus, ForgeChecks, ForgeChecksState, ForgeKind, ForgeRepoRef};
use serde_json::Value;

use crate::error::{ForgeError, Result};

/// Validates a repository reference before it is used in a URL.
///
/// Owners are single path segments everywhere except GitLab, where a namespace
/// may contain `/`. Each segment must be non-empty and must not be `.` or
/// `..`, and no segment may contain a percent-encoded slash trick.
pub fn validate_repo(repo: &ForgeRepoRef) -> Result<()> {
    if repo.base_url.trim().is_empty() {
        return Err(ForgeError::InvalidRequest(
            "the repository has no base URL".into(),
        ));
    }
    let owner_segments = repo.owner.split('/').collect::<Vec<_>>();
    if repo.forge != ForgeKind::GitLab && owner_segments.len() != 1 {
        return Err(ForgeError::InvalidRequest(format!(
            "{} does not support nested owners",
            repo.forge.display_name()
        )));
    }
    if owner_segments.is_empty() {
        return Err(ForgeError::InvalidRequest("the owner is empty".into()));
    }
    for segment in owner_segments
        .iter()
        .chain(std::iter::once(&repo.repo.as_str()))
    {
        validate_segment(segment)?;
    }
    Ok(())
}

fn validate_segment(segment: &str) -> Result<()> {
    if segment.is_empty()
        || segment == "."
        || segment == ".."
        || segment.contains('/')
        || segment.contains('\\')
        || segment.contains('?')
        || segment.contains('#')
        || segment.contains('%')
    {
        return Err(ForgeError::InvalidRequest(format!(
            "invalid repository path segment {segment:?}"
        )));
    }
    Ok(())
}

/// Percent-encodes one GitLab project path segment (`owner/repo` becomes
/// `owner%2Frepo`).
pub fn encode_project_path(repo: &ForgeRepoRef) -> String {
    repo.full_name().replace('%', "%25").replace('/', "%2F")
}

/// Parses an RFC 3339 timestamp from a JSON string.
pub fn parse_datetime(value: &Value, field: &str) -> Result<DateTime<Utc>> {
    let text = value
        .as_str()
        .ok_or_else(|| ForgeError::Parse(format!("{field} is not a string")))?;
    DateTime::parse_from_rfc3339(text)
        .map(|value| value.with_timezone(&Utc))
        .map_err(|error| ForgeError::Parse(format!("{field} is not RFC 3339: {error}")))
}

/// Parses an optional RFC 3339 timestamp.
pub fn parse_optional_datetime(
    value: Option<&Value>,
    field: &str,
) -> Result<Option<DateTime<Utc>>> {
    match value {
        None | Some(Value::Null) => Ok(None),
        Some(value) => parse_datetime(value, field).map(Some),
    }
}

/// Returns the string at `key`, or an empty string.
pub fn text(value: &Value, key: &str) -> String {
    value
        .get(key)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned()
}

/// Returns an optional string at `key`.
pub fn optional_text(value: &Value, key: &str) -> Option<String> {
    value
        .get(key)
        .and_then(Value::as_str)
        .map(ToOwned::to_owned)
}

/// Fills the aggregate counters and state from a list of checks.
pub fn finish_checks(
    repo: &ForgeRepoRef,
    number: u32,
    head_sha: String,
    checks: Vec<h4rn_protocol::ForgeCheck>,
) -> ForgeChecks {
    let total = checks.len() as u32;
    let passed = checks
        .iter()
        .filter(|check| check.status.is_success())
        .count() as u32;
    let failed = checks
        .iter()
        .filter(|check| check.status.is_failure())
        .count() as u32;
    let pending = checks
        .iter()
        .filter(|check| check.status.is_pending())
        .count() as u32;
    let state = if total == 0 {
        ForgeChecksState::Unknown
    } else if failed > 0 {
        ForgeChecksState::Failing
    } else if pending > 0 {
        ForgeChecksState::Pending
    } else if passed == total {
        ForgeChecksState::Passing
    } else {
        ForgeChecksState::Unknown
    };
    ForgeChecks {
        repo: repo.clone(),
        number,
        head_sha,
        state,
        total,
        passed,
        failed,
        pending,
        checks,
        updated_at: Utc::now(),
    }
}

/// Maps one of GitHub's check-run conclusions or statuses.
pub fn github_check_status(status: &str, conclusion: Option<&str>) -> ForgeCheckStatus {
    match conclusion.unwrap_or_default() {
        "success" => return ForgeCheckStatus::Success,
        "failure" => return ForgeCheckStatus::Failure,
        "neutral" => return ForgeCheckStatus::Neutral,
        "cancelled" => return ForgeCheckStatus::Cancelled,
        "skipped" => return ForgeCheckStatus::Skipped,
        "timed_out" => return ForgeCheckStatus::TimedOut,
        "action_required" => return ForgeCheckStatus::ActionRequired,
        _ => {}
    }
    match status {
        "queued" => ForgeCheckStatus::Queued,
        "in_progress" => ForgeCheckStatus::InProgress,
        "completed" => ForgeCheckStatus::Unknown,
        "pending" => ForgeCheckStatus::Queued,
        "success" => ForgeCheckStatus::Success,
        "failure" | "error" => ForgeCheckStatus::Failure,
        _ => ForgeCheckStatus::Unknown,
    }
}

/// Maps a GitLab pipeline or job status.
pub fn gitlab_check_status(status: &str) -> ForgeCheckStatus {
    match status {
        "created" | "pending" | "preparing" | "scheduled" => ForgeCheckStatus::Queued,
        "running" => ForgeCheckStatus::InProgress,
        "success" => ForgeCheckStatus::Success,
        "failed" => ForgeCheckStatus::Failure,
        "canceled" | "cancelled" => ForgeCheckStatus::Cancelled,
        "skipped" => ForgeCheckStatus::Skipped,
        "manual" => ForgeCheckStatus::ActionRequired,
        "waiting_for_resource" | "blocked" => ForgeCheckStatus::Queued,
        _ => ForgeCheckStatus::Unknown,
    }
}

/// Maps a Gitea/Forgejo commit status.
pub fn gitea_check_status(status: &str) -> ForgeCheckStatus {
    match status {
        "pending" | "running" => ForgeCheckStatus::InProgress,
        "success" => ForgeCheckStatus::Success,
        "failure" | "error" => ForgeCheckStatus::Failure,
        "warning" => ForgeCheckStatus::Neutral,
        "skipped" => ForgeCheckStatus::Skipped,
        _ => ForgeCheckStatus::Unknown,
    }
}

/// Milliseconds between two timestamps, when both exist.
pub fn duration_ms(start: Option<&DateTime<Utc>>, end: Option<&DateTime<Utc>>) -> Option<u64> {
    let start = start?;
    let end = end?;
    let millis = (*end - *start).num_milliseconds();
    (millis >= 0).then_some(millis as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repo_validation_rejects_traversal() {
        let mut repo = ForgeRepoRef {
            forge: ForgeKind::GitHub,
            base_url: "https://github.com".into(),
            owner: "..".into(),
            repo: "api".into(),
        };
        assert!(validate_repo(&repo).is_err());
        repo.owner = "team".into();
        assert!(validate_repo(&repo).is_ok());

        let gitlab = ForgeRepoRef {
            forge: ForgeKind::GitLab,
            base_url: "https://gitlab.com".into(),
            owner: "team/platform".into(),
            repo: "api".into(),
        };
        assert!(validate_repo(&gitlab).is_ok());
        assert_eq!(encode_project_path(&gitlab), "team%2Fplatform%2Fapi");
    }

    #[test]
    fn aggregate_state_prefers_failures_then_pending() {
        let repo = ForgeRepoRef {
            forge: ForgeKind::GitHub,
            base_url: "https://github.com".into(),
            owner: "o".into(),
            repo: "r".into(),
        };
        let mut checks = finish_checks(&repo, 1, "abc".into(), vec![]);
        assert_eq!(checks.state, ForgeChecksState::Unknown);

        checks.checks.push(h4rn_protocol::ForgeCheck {
            name: "a".into(),
            status: ForgeCheckStatus::Success,
            details_url: None,
            description: None,
            started_at: None,
            completed_at: None,
            duration_ms: None,
        });
        checks.checks.push(h4rn_protocol::ForgeCheck {
            name: "b".into(),
            status: ForgeCheckStatus::InProgress,
            details_url: None,
            description: None,
            started_at: None,
            completed_at: None,
            duration_ms: None,
        });
        let finished = finish_checks(&repo, 1, "abc".into(), checks.checks.clone());
        assert_eq!(finished.state, ForgeChecksState::Pending);
        assert_eq!(finished.pending, 1);

        let mut failing = finished.clone();
        failing.checks[1].status = ForgeCheckStatus::Failure;
        let failed = finish_checks(&repo, 1, "abc".into(), failing.checks);
        assert_eq!(failed.state, ForgeChecksState::Failing);
    }
}
