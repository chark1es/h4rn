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

/// Length of the content fingerprint a path key carries: a BLAKE3 digest in
/// lowercase hex.
const PATH_KEY_FINGERPRINT_HEX_LEN: usize = 64;

/// The parts a path key pins, when it has the exact shape
/// [`path_source_key`] writes.
///
/// A path key is `source:path:<path>:<size>:<fingerprint>`. The path may itself
/// contain colons, so the size and fingerprint are read from the right. The
/// fingerprint is a BLAKE3 digest written in lowercase hex, so a session id
/// that happens to contain colons and numbers is not mistaken for a path key.
fn path_key_parts(key: &str) -> Option<(u64, &str)> {
    let (_, rest) = key.split_once(':')?;
    let tail = rest.strip_prefix("path:")?;
    let (path_and_size, fingerprint) = tail.rsplit_once(':')?;
    let (path, size) = path_and_size.rsplit_once(':')?;
    if path.is_empty()
        || fingerprint.len() != PATH_KEY_FINGERPRINT_HEX_LEN
        || !fingerprint
            .bytes()
            .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
    {
        return None;
    }
    Some((size.parse().ok()?, fingerprint))
}

/// The file size a path key pins, when it has the full path-key shape.
///
/// Keys without that exact suffix are not path keys: no daemon ever wrote a
/// shorter one, and a session id can spell anything.
pub fn path_keyed_source_size(key: &str) -> Option<u64> {
    path_key_parts(key).map(|(size, _)| size)
}

/// The content fingerprint a path key pins, when it has the full path-key shape.
pub fn path_keyed_source_fingerprint(key: &str) -> Option<&str> {
    path_key_parts(key).map(|(_, fingerprint)| fingerprint)
}

/// Whether a stored source key names its transcript by path.
///
/// A path key carries the file's size and content fingerprint, so it pins one
/// version of the file: a transcript that keeps growing is a new conversation.
/// A session-id key names the conversation itself, so it still recognises a
/// transcript whose head read did not reveal the id. Only the full
/// `:path:<path>:<size>:<fingerprint>` shape counts, so a session id that itself
/// begins with `path:` is still a session id.
pub fn path_keyed_source_key(key: &str) -> bool {
    path_key_parts(key).is_some()
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

    /// A fingerprint shaped like the BLAKE3 digest a real path key writes.
    fn fingerprint() -> String {
        "a".repeat(PATH_KEY_FINGERPRINT_HEX_LEN)
    }

    #[test]
    fn a_path_key_is_told_apart_from_a_session_key() {
        assert!(path_keyed_source_key(&path_source_key(
            ImportSource::Codex,
            "/tmp/a.jsonl",
            12,
            &fingerprint()
        )));
        assert!(!path_keyed_source_key(&source_key(
            ImportSource::ClaudeCode,
            Some("abc"),
            "/tmp/a.jsonl"
        )));
        assert!(!path_keyed_source_key("codex:path-with-a-colon"));
    }

    #[test]
    fn a_path_key_needs_its_size_and_fingerprint_suffix() {
        // The old unsuffixed path key is not a path key: it cannot be told
        // apart from a session id that happens to spell `path:`.
        assert!(!path_keyed_source_key(&source_key(
            ImportSource::Codex,
            None,
            "/tmp/a.jsonl"
        )));
        assert_eq!(
            path_keyed_source_size(&source_key(ImportSource::Codex, None, "/tmp/a.jsonl")),
            None
        );

        // A session id that itself begins with `path:` is a session id.
        let session = source_key(ImportSource::ClaudeCode, Some("path:abc"), "/tmp/a.jsonl");
        assert!(!path_keyed_source_key(&session));
        assert_eq!(path_keyed_source_size(&session), None);

        // A path key without both parts is not one either.
        assert!(!path_keyed_source_key("codex:path:/tmp/a.jsonl:12"));
        assert!(!path_keyed_source_key("codex:path:/tmp/a.jsonl:abc:def"));
    }

    #[test]
    fn a_session_id_shaped_like_a_path_key_is_still_a_session_id() {
        // `path:vendor:42:nightly` has a `path:` prefix, a number and a final
        // word, but the word is not the 64-character hex digest a path key
        // writes. Treating it as a path key would let a late id be offered
        // again, so only the exact written shape counts.
        let session = source_key(
            ImportSource::ClaudeCode,
            Some("path:vendor:42:nightly"),
            "/tmp/a.jsonl",
        );
        assert!(!path_keyed_source_key(&session));
        assert_eq!(path_keyed_source_size(&session), None);
        assert_eq!(path_keyed_source_fingerprint(&session), None);

        // A digest-shaped suffix of the wrong length is not one either.
        let short = format!("codex:path:/tmp/a.jsonl:42:{}", "f".repeat(32));
        assert!(!path_keyed_source_key(&short));
        // Nor is one with characters outside lowercase hex.
        let not_hex = format!("codex:path:/tmp/a.jsonl:42:{}", "g".repeat(64));
        assert!(!path_keyed_source_key(&not_hex));
    }

    #[test]
    fn a_path_key_reads_its_size_even_when_the_path_has_colons() {
        let fingerprint = fingerprint();
        let key = path_source_key(ImportSource::Codex, "/tmp/a:b/c.jsonl", 4096, &fingerprint);
        assert!(path_keyed_source_key(&key));
        assert_eq!(path_keyed_source_size(&key), Some(4096));
        assert_eq!(
            path_keyed_source_fingerprint(&key),
            Some(fingerprint.as_str())
        );
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
