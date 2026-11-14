//! Renders a pull request into the compact markdown block handed to an agent.
//!
//! The handoff is deliberately small: a handful of lines of metadata, the PR
//! body, the unresolved review threads, and the checks that are failing or
//! still running. Everything else (the full diff, resolved threads, passing
//! checks) stays out so the first turn fits in a normal context window.
//! The agent can fetch more through the PR tools.

use std::fmt::Write as _;

use h4rn_protocol::{
    ForgeCheckStatus, ForgeChecks, ForgeChecksState, ForgeDiffSide, ForgeMergeable, ForgePrState,
    ForgePullRequest, ForgeReviewThread,
};

/// Maximum characters of the PR body included.
const MAX_BODY: usize = 4_000;
/// Maximum characters of one review comment included.
const MAX_COMMENT: usize = 1_200;
/// Maximum comment chars across the whole context block.
const MAX_COMMENT_TOTAL: usize = 12_000;

/// Renders the agent context block for one PR.
///
/// `threads` and `checks` are whatever the daemon last fetched; a failed
/// fetch should be represented by an empty thread list or an unknown checks
/// snapshot rather than blocking the handoff entirely.
pub fn pr_context_for_agent(
    pr: &ForgePullRequest,
    threads: &[ForgeReviewThread],
    checks: &ForgeChecks,
) -> String {
    let summary = &pr.summary;
    let mut out = String::new();

    let _ = writeln!(
        out,
        "# {} #{}: {}",
        summary.repo.repo, summary.number, summary.title
    );
    let _ = writeln!(out);
    let _ = writeln!(
        out,
        "- Repository: `{}` ({})",
        summary.repo.full_name(),
        summary.repo.forge.display_name()
    );
    let _ = writeln!(out, "- URL: {}", summary.web_url);
    let _ = writeln!(
        out,
        "- Author: @{} · Branch: `{}` → `{}`",
        summary.author, summary.head_branch, summary.base_branch
    );
    let mut state = state_label(summary.state).to_owned();
    if matches!(pr.mergeable, ForgeMergeable::Conflicting) {
        state.push_str(" · merge conflicts");
    } else if matches!(pr.mergeable, ForgeMergeable::Mergeable) {
        state.push_str(" · mergeable");
    }
    let _ = writeln!(
        out,
        "- State: {state} · Diff: +{} −{} across {} file{}",
        pr.additions,
        pr.deletions,
        pr.changed_files,
        if pr.changed_files == 1 { "" } else { "s" }
    );
    if let Some(head) = &summary.head_sha {
        let short = &head[..head.len().min(12)];
        let _ = writeln!(out, "- Head commit: `{short}`");
    }

    let reviews = review_lines(pr);
    if !reviews.is_empty() {
        let _ = writeln!(out, "- Reviews: {}", reviews.join(", "));
    }

    let _ = writeln!(out);
    let _ = writeln!(out, "## Description");
    let _ = writeln!(out);
    let body = pr.body.trim();
    if body.is_empty() {
        let _ = writeln!(out, "_(no description)_");
    } else {
        let _ = writeln!(out, "{}", truncate(body, MAX_BODY));
    }

    let unresolved: Vec<&ForgeReviewThread> =
        threads.iter().filter(|thread| !thread.resolved).collect();
    let resolved = threads.len().saturating_sub(unresolved.len());
    let _ = writeln!(out);
    if unresolved.is_empty() {
        let _ = writeln!(out, "## Unresolved review threads");
        let _ = writeln!(out);
        if resolved > 0 {
            let _ = writeln!(
                out,
                "_(none; {resolved} resolved thread{} omitted)_",
                plural(resolved)
            );
        } else {
            let _ = writeln!(out, "_(none)_");
        }
    } else {
        let _ = writeln!(out, "## Unresolved review threads ({})", unresolved.len());
        let mut spent = 0usize;
        for thread in unresolved {
            let _ = writeln!(out);
            let side = side_label(thread.side);
            let location = match thread.line {
                Some(line) => format!("`{}:{line}`", thread.path),
                None => format!("`{}`", thread.path),
            };
            let _ = writeln!(out, "### {location} ({side})");
            for comment in &thread.comments {
                if spent >= MAX_COMMENT_TOTAL {
                    let _ = writeln!(out, "- _further comments omitted_");
                    break;
                }
                let body = truncate(comment.body.trim(), MAX_COMMENT);
                spent = spent.saturating_add(body.len());
                let _ = writeln!(out, "- **@{}**: {body}", comment.author);
            }
            if thread.outdated {
                let _ = writeln!(out, "- _the anchored code is outdated_");
            }
        }
    }

    let failing: Vec<_> = checks
        .checks
        .iter()
        .filter(|check| check.status.is_failure())
        .collect();
    let pending: Vec<_> = checks
        .checks
        .iter()
        .filter(|check| check.status.is_pending())
        .collect();
    let passing = checks
        .checks
        .iter()
        .filter(|c| c.status.is_success())
        .count();

    let _ = writeln!(out);
    let _ = writeln!(out, "## Checks");
    let _ = writeln!(out);
    if checks.state == ForgeChecksState::Unknown && checks.total == 0 {
        let _ = writeln!(out, "_(no checks reported)_");
    } else {
        let _ = writeln!(
            out,
            "{} · {} passed · {} failed · {} pending",
            checks_state_label(checks.state),
            checks.passed,
            checks.failed,
            checks.pending
        );
        if !failing.is_empty() {
            let _ = writeln!(out);
            let _ = writeln!(out, "### Failing");
            for check in &failing {
                let _ = writeln!(out, "- {}", check_line(check));
            }
        }
        if !pending.is_empty() {
            let _ = writeln!(out);
            let _ = writeln!(out, "### Running");
            for check in &pending {
                let _ = writeln!(out, "- {}", check_line(check));
            }
        }
        if passing > 0 && failing.is_empty() && pending.is_empty() {
            let _ = writeln!(out, "- all {passing} checks passed");
        }
    }

    out.trim_end().to_owned() + "\n"
}

fn check_line(check: &h4rn_protocol::ForgeCheck) -> String {
    let status = check_status_label(check.status);
    let mut line = format!("`{}` — {status}", check.name);
    if let Some(description) = check.description.as_deref().filter(|text| !text.is_empty()) {
        line.push_str(" — ");
        line.push_str(description);
    }
    if let Some(url) = &check.details_url {
        line.push_str(" — ");
        line.push_str(url);
    }
    line
}

fn review_lines(pr: &ForgePullRequest) -> Vec<String> {
    let mut lines = Vec::new();
    for review in &pr.reviewers {
        let label = match review.state {
            h4rn_protocol::ForgeReviewState::Approved => "approved",
            h4rn_protocol::ForgeReviewState::ChangesRequested => "changes requested",
            h4rn_protocol::ForgeReviewState::Commented => "commented",
            h4rn_protocol::ForgeReviewState::Pending => "pending",
            h4rn_protocol::ForgeReviewState::Dismissed => "dismissed",
        };
        lines.push(format!("@{} {label}", review.author));
    }
    lines
}

fn state_label(state: ForgePrState) -> &'static str {
    match state {
        ForgePrState::Open => "open",
        ForgePrState::Draft => "draft",
        ForgePrState::Merged => "merged",
        ForgePrState::Closed => "closed",
    }
}

fn side_label(side: ForgeDiffSide) -> &'static str {
    match side {
        ForgeDiffSide::Left => "left",
        ForgeDiffSide::Right => "right",
        ForgeDiffSide::Unknown => "side unknown",
    }
}

fn checks_state_label(state: ForgeChecksState) -> &'static str {
    match state {
        ForgeChecksState::Passing => "passing",
        ForgeChecksState::Failing => "failing",
        ForgeChecksState::Pending => "pending",
        ForgeChecksState::Unknown => "unknown",
    }
}

fn check_status_label(status: ForgeCheckStatus) -> &'static str {
    match status {
        ForgeCheckStatus::Queued => "queued",
        ForgeCheckStatus::InProgress => "running",
        ForgeCheckStatus::Success => "success",
        ForgeCheckStatus::Failure => "failure",
        ForgeCheckStatus::Neutral => "neutral",
        ForgeCheckStatus::Cancelled => "cancelled",
        ForgeCheckStatus::Skipped => "skipped",
        ForgeCheckStatus::TimedOut => "timed out",
        ForgeCheckStatus::ActionRequired => "action required",
        ForgeCheckStatus::Unknown => "unknown",
    }
}

fn plural(count: usize) -> &'static str {
    if count == 1 { "" } else { "s" }
}

fn truncate(value: &str, max: usize) -> String {
    if value.chars().count() <= max {
        return value.to_owned();
    }
    let mut text: String = value.chars().take(max.saturating_sub(1)).collect();
    text.push('…');
    text
}

#[cfg(test)]
mod tests {
    use chrono::Utc;
    use h4rn_protocol::{
        ForgeCheck, ForgeChecksState, ForgeDiffSide, ForgeKind, ForgePrSummary, ForgeRepoRef,
        ForgeReview, ForgeReviewComment, ForgeReviewState,
    };

    use super::*;

    fn repo() -> ForgeRepoRef {
        ForgeRepoRef {
            forge: ForgeKind::GitHub,
            base_url: "https://github.com".into(),
            owner: "acme".into(),
            repo: "widget".into(),
        }
    }

    fn summary() -> ForgePrSummary {
        ForgePrSummary {
            repo: repo(),
            number: 42,
            title: "Fix the widget".into(),
            state: ForgePrState::Open,
            author: "alice".into(),
            head_branch: "alice/fix".into(),
            base_branch: "main".into(),
            head_sha: Some("abcdef0123456789".into()),
            updated_at: Utc::now(),
            web_url: "https://github.com/acme/widget/pull/42".into(),
            checks: ForgeChecksState::Failing,
            review: ForgeReviewState::ChangesRequested,
            draft: false,
        }
    }

    fn pr() -> ForgePullRequest {
        ForgePullRequest {
            summary: summary(),
            body: "Fixes the widget.".into(),
            labels: Vec::new(),
            mergeable: ForgeMergeable::Conflicting,
            base_sha: None,
            additions: 12,
            deletions: 3,
            changed_files: 2,
            commits: 1,
            created_at: Utc::now(),
            reviewers: vec![ForgeReview {
                author: "bob".into(),
                state: ForgeReviewState::ChangesRequested,
                submitted_at: None,
                body: None,
            }],
            merge_commit_sha: None,
        }
    }

    fn thread(resolved: bool) -> ForgeReviewThread {
        ForgeReviewThread {
            thread_id: "t1".into(),
            path: "src/widget.rs".into(),
            line: Some(12),
            side: ForgeDiffSide::Right,
            resolved,
            outdated: false,
            comments: vec![ForgeReviewComment {
                comment_id: "c1".into(),
                thread_id: Some("t1".into()),
                author: "bob".into(),
                body: "This can panic on empty input.".into(),
                created_at: Utc::now(),
                updated_at: None,
                web_url: None,
                path: "src/widget.rs".into(),
                line: Some(12),
                side: ForgeDiffSide::Right,
                in_reply_to: None,
            }],
        }
    }

    fn checks() -> ForgeChecks {
        ForgeChecks {
            repo: repo(),
            number: 42,
            head_sha: "abcdef".into(),
            state: ForgeChecksState::Failing,
            total: 2,
            passed: 1,
            failed: 1,
            pending: 0,
            checks: vec![
                ForgeCheck {
                    name: "ci/test".into(),
                    status: ForgeCheckStatus::Failure,
                    details_url: Some("https://example.test/run/1".into()),
                    description: Some("2 tests failed".into()),
                    started_at: None,
                    completed_at: None,
                    duration_ms: None,
                    required: None,
                },
                ForgeCheck {
                    name: "ci/lint".into(),
                    status: ForgeCheckStatus::Success,
                    details_url: None,
                    description: None,
                    started_at: None,
                    completed_at: None,
                    duration_ms: None,
                    required: None,
                },
            ],
            rerun: crate::rerun_support(ForgeKind::GitHub),
            updated_at: Utc::now(),
        }
    }

    #[test]
    fn renders_metadata_threads_and_failing_checks() {
        let text = pr_context_for_agent(&pr(), &[thread(false), thread(true)], &checks());
        assert!(text.contains("# widget #42: Fix the widget"));
        assert!(text.contains("Branch: `alice/fix` → `main`"));
        assert!(text.contains("Diff: +12 −3 across 2 files"));
        assert!(text.contains("merge conflicts"));
        assert!(text.contains("@bob changes requested"));
        assert!(text.contains("## Unresolved review threads (1)"));
        assert!(text.contains("`src/widget.rs:12` (right)"));
        assert!(text.contains("This can panic on empty input."));
        assert!(text.contains("### Failing"));
        assert!(text.contains("ci/test"));
        assert!(!text.contains("ci/lint"));
    }

    #[test]
    fn omits_noise_and_truncates_long_bodies() {
        let mut pr = pr();
        pr.body = "§".repeat(MAX_BODY + 100);
        let text = pr_context_for_agent(&pr, &[], &checks());
        assert!(!text.contains("Resolved review threads"));
        assert!(text.contains("_(none)_"));
        assert!(text.chars().filter(|c| *c == '§').count() == MAX_BODY - 1);
        assert!(text.contains('…'));
    }
}
