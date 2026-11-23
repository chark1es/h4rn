//! Importing conversations that Claude Code and Codex already saved on this
//! machine.
//!
//! Discovery and parsing live in the daemon; this module is only the wire
//! contract: what was found, which projects the person picked, and how far a
//! running import has got.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use ts_rs::TS;

use crate::common::HarnessKind;

/// A harness whose saved conversations can be imported.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, TS)]
#[serde(rename_all = "snake_case")]
pub enum ImportSource {
    /// Claude Code transcripts under `<CLAUDE_CONFIG_DIR or ~/.claude>/projects`.
    ClaudeCode,
    /// Codex rollouts under `<CODEX_HOME or ~/.codex>/sessions`.
    Codex,
}

impl ImportSource {
    /// The harness a conversation continues on.
    pub fn harness(self) -> HarnessKind {
        match self {
            Self::ClaudeCode => HarnessKind::ClaudeCode,
            Self::Codex => HarnessKind::Codex,
        }
    }

    /// The spelling used in a dedupe key.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ClaudeCode => "claude_code",
            Self::Codex => "codex",
        }
    }
}

/// One project found on disk, grouped by git checkout.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct ImportProject {
    /// Stable grouping key: a GitHub `owner/name`, or the main checkout path.
    pub key: String,
    /// Main checkout path, or the folder itself for a non-git project.
    pub path: String,
    /// Display name: `owner/name` for GitHub, otherwise the folder name.
    pub name: String,
    /// Whether the folder is a git work tree.
    pub git: bool,
    /// Newest conversation activity in the group.
    pub last_active: DateTime<Utc>,
    /// Conversations found in this project.
    pub conversations: u32,
    /// Conversations that have not been imported yet.
    pub importable: u32,
    /// Whether this project is offered already checked.
    ///
    /// A git repository with at least three conversations, active in the last
    /// 30 days, that is not already registered as a project in h4rn.
    pub recommended: bool,
}

/// Everything one scan found.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct ImportScan {
    /// Projects, newest activity first.
    pub projects: Vec<ImportProject>,
    /// Limits that were hit or folders that could not be read.
    pub warnings: Vec<String>,
    /// When the scan ran.
    pub scanned_at: DateTime<Utc>,
}

/// How far an import has got.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct ImportProgress {
    /// Identifies one import run.
    pub import_id: String,
    /// Whether the run is still going.
    pub running: bool,
    /// Projects finished.
    pub projects_done: u32,
    /// Projects selected.
    pub projects_total: u32,
    /// Conversations finished.
    pub conversations_done: u32,
    /// Conversations selected.
    pub conversations_total: u32,
    /// New threads created.
    pub imported: u32,
    /// Conversations already imported, skipped, or empty.
    pub skipped: u32,
    /// Conversations that could not be imported.
    pub failed: u32,
    /// Project currently being imported, for the progress line.
    pub current_project: Option<String>,
    /// Limits hit during this run.
    pub warnings: Vec<String>,
    /// When the run finished; absent while it is still running.
    pub finished_at: Option<DateTime<Utc>>,
}

impl ImportProgress {
    /// A fresh run with nothing done yet.
    pub fn starting(import_id: String, projects_total: u32, conversations_total: u32) -> Self {
        Self {
            import_id,
            running: true,
            projects_done: 0,
            projects_total,
            conversations_done: 0,
            conversations_total,
            imported: 0,
            skipped: 0,
            failed: 0,
            current_project: None,
            warnings: Vec::new(),
            finished_at: None,
        }
    }
}

/// A transient import progress frame.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct ImportEvent {
    /// The latest state of the run.
    pub progress: ImportProgress,
}

/// One conversation's source identity, stored so it is imported once.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct ImportedSource {
    /// Dedupe key: source plus session id, or the transcript path.
    pub key: String,
    /// Thread the conversation became.
    pub thread_id: crate::ids::ThreadId,
    /// Transcript the conversation came from.
    pub path: String,
    /// When it was imported.
    pub imported_at: DateTime<Utc>,
}

/// Parses the same source key the daemon writes.
pub fn source_key(source: ImportSource, session_id: Option<&str>, path: &str) -> String {
    match session_id.map(str::trim).filter(|id| !id.is_empty()) {
        Some(id) => format!("{}:{id}", source.as_str()),
        None => format!("{}:path:{path}", source.as_str()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn source_keys_prefer_the_session_id() {
        assert_eq!(
            source_key(ImportSource::ClaudeCode, Some("abc"), "/tmp/a.jsonl"),
            "claude_code:abc"
        );
        assert_eq!(
            source_key(ImportSource::Codex, None, "/tmp/a.jsonl"),
            "codex:path:/tmp/a.jsonl"
        );
    }

    #[test]
    fn sources_name_the_harness_they_continue_on() {
        assert_eq!(
            ImportSource::ClaudeCode.harness(),
            crate::HarnessKind::ClaudeCode
        );
        assert_eq!(ImportSource::Codex.harness(), crate::HarnessKind::Codex);
    }
}
