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
    #[serde(default)]
    pub git: bool,
    /// Newest conversation activity in the group.
    pub last_active: DateTime<Utc>,
    /// Conversations found in this project.
    #[serde(default)]
    pub conversations: u32,
    /// Conversations that have not been imported yet.
    #[serde(default)]
    pub importable: u32,
    /// Whether this project is offered already checked.
    ///
    /// A git repository with at least three conversations, active in the last
    /// 30 days, that is not already registered as a project in h4rn.
    #[serde(default)]
    pub recommended: bool,
}

/// Everything one scan found.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct ImportScan {
    /// Projects, newest activity first.
    #[serde(default)]
    pub projects: Vec<ImportProject>,
    /// Limits that were hit or folders that could not be read.
    #[serde(default)]
    pub warnings: Vec<String>,
    /// When the scan ran.
    #[serde(default = "Utc::now")]
    pub scanned_at: DateTime<Utc>,
}

fn default_import_id() -> String {
    String::new()
}

/// How far an import has got.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct ImportProgress {
    /// Identifies one import run.
    #[serde(default = "default_import_id")]
    pub import_id: String,
    /// Whether the run is still going.
    #[serde(default)]
    pub running: bool,
    /// Projects finished.
    #[serde(default)]
    pub projects_done: u32,
    /// Projects selected.
    #[serde(default)]
    pub projects_total: u32,
    /// Conversations finished.
    #[serde(default)]
    pub conversations_done: u32,
    /// Conversations selected.
    #[serde(default)]
    pub conversations_total: u32,
    /// New threads created.
    #[serde(default)]
    pub imported: u32,
    /// Conversations already imported, skipped, or empty.
    #[serde(default)]
    pub skipped: u32,
    /// Conversations that could not be imported.
    #[serde(default)]
    pub failed: u32,
    /// Project currently being imported, for the progress line.
    #[serde(default)]
    pub current_project: Option<String>,
    /// Limits hit during this run.
    #[serde(default)]
    pub warnings: Vec<String>,
    /// When the run finished; absent while it is still running.
    #[serde(default)]
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
    #[serde(default)]
    pub path: String,
    /// When it was imported.
    #[serde(default = "Utc::now")]
    pub imported_at: DateTime<Utc>,
}

/// Parses the same source key the daemon writes.
pub fn source_key(source: ImportSource, session_id: Option<&str>, path: &str) -> String {
    match session_id.map(str::trim).filter(|id| !id.is_empty()) {
        Some(id) => format!("{}:{id}", source.as_str()),
        None => format!("{}:path:{path}", source.as_str()),
    }
}

/// The key for a transcript that names no session id.
///
/// Its identity is the path plus the file's size and content fingerprint, so a
/// rewritten transcript is a new conversation instead of a silent duplicate.
pub fn path_source_key(source: ImportSource, path: &str, size: u64, fingerprint: &str) -> String {
    format!("{}:path:{path}:{size}:{fingerprint}", source.as_str())
}

/// Whether a stored source key names its transcript by path.
///
/// A path key carries the file's size and content fingerprint, so it pins one
/// version of the file: a transcript that keeps growing is a new conversation.
/// A session-id key names the conversation itself, so it still recognises a
/// transcript whose head read did not reveal the id.
pub fn path_keyed_source_key(key: &str) -> bool {
    key.split_once(':')
        .is_some_and(|(_, rest)| rest.starts_with("path:"))
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
    fn a_path_key_is_told_apart_from_a_session_key() {
        assert!(path_keyed_source_key(&source_key(
            ImportSource::Codex,
            None,
            "/tmp/a.jsonl"
        )));
        assert!(path_keyed_source_key(&path_source_key(
            ImportSource::Codex,
            "/tmp/a.jsonl",
            12,
            "fingerprint"
        )));
        assert!(!path_keyed_source_key(&source_key(
            ImportSource::ClaudeCode,
            Some("abc"),
            "/tmp/a.jsonl"
        )));
        assert!(!path_keyed_source_key("codex:path-with-a-colon"));
    }

    #[test]
    fn an_older_progress_shape_still_deserialises() {
        // A daemon from before warnings and the current project existed.
        let progress: ImportProgress = serde_json::from_str(
            r#"{"import_id":"imp_1","running":false,"projects_done":1,"projects_total":1,
                "conversations_done":2,"conversations_total":2,"imported":2,"skipped":0,
                "failed":0,"finished_at":null}"#,
        )
        .expect("older progress");
        assert!(progress.warnings.is_empty());
        assert_eq!(progress.current_project, None);

        // A scan that predates warnings.
        let scan: ImportScan =
            serde_json::from_str(r#"{"projects":[],"scanned_at":"2026-11-20T10:00:00Z"}"#)
                .expect("older scan");
        assert!(scan.warnings.is_empty());
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
