//! Codex rollouts: `<CODEX_HOME>/sessions/YYYY/MM/DD/rollout-*.jsonl`.
//!
//! Records are a mix of `session_meta`, `turn_context`, canonical `event_msg`
//! user turns and `response_item` messages. Codex writes the same user turn
//! more than once: a canonical event, a response copy, and generated setup
//! context such as the environment block or the project's private AGENTS.md
//! instructions. Canonical events usually carry no turn id, so the turn id on
//! the response copies is what ties them together.
//!
//! Dedupe follows t3code's two-pass rule. Within one turn:
//!
//! 1. When a canonical event's text matches a response copy that names a valid
//!    turn id, that turn is canonical: every response user record with that id
//!    is dropped, setup context included.
//! 2. Any remaining response copy whose text exactly matches the canonical
//!    event is dropped.
//!
//! A turn with no canonical event is response-only, so its context stays. A
//! response copy whose turn id names another turn is kept, so a repeated prompt
//! after an aborted turn loses neither turn's text.
//!
//! The persisted turn boundaries (`turn_context`, `turn_aborted`, and the
//! `task_*` events) end the pending window: a canonical prompt and its copy
//! belong to the turn they were written in, never to a later one.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use h4rn_protocol::{ImportSource, MessageRole};
use serde::Deserialize;

use super::record::{Content, Record, RecordReader, maybe_text};
use super::{
    History, KeptMessage, ParsedConversation, Peek, SkipReason, derive_title, open_reader,
    parse_time, read_line_bounded,
};

/// Reads just enough of a rollout to group it by folder.
pub(crate) fn peek(path: &Path) -> Option<Peek> {
    let mut reader = open_reader(path).ok()?;
    let mut line = Vec::new();
    let mut records = 0;
    let mut cwd: Option<PathBuf> = None;
    let mut session_id: Option<String> = None;
    let mut has_visible = false;
    while let Ok(Some(complete)) = read_line_bounded(&mut reader, &mut line) {
        records += 1;
        if records > 200 {
            break;
        }
        if !complete {
            continue;
        }
        let Ok(record) = serde_json::from_slice::<TranscriptRecord>(&line) else {
            continue;
        };
        if cwd.is_none()
            && let Some(value) = record.cwd()
        {
            cwd = Some(PathBuf::from(value));
        }
        if session_id.is_none() {
            session_id = record.session_id();
        }
        if !has_visible && record.is_visible_message() {
            has_visible = true;
        }
    }
    let cwd = cwd?;
    Some(Peek {
        cwd,
        session_id,
        has_visible,
    })
}

/// One JSONL record, with only the fields the import reads.
#[derive(Debug, Default, Deserialize)]
struct TranscriptRecord {
    #[serde(rename = "type")]
    kind: Option<String>,
    timestamp: Option<String>,
    cwd: Option<String>,
    payload: Option<Payload>,
}

#[derive(Debug, Default, Deserialize)]
struct Payload {
    #[serde(rename = "type")]
    kind: Option<String>,
    id: Option<String>,
    #[serde(rename = "session_id")]
    session_id: Option<String>,
    #[serde(default, deserialize_with = "maybe_text")]
    message: Option<String>,
    model: Option<String>,
    cwd: Option<String>,
    role: Option<String>,
    turn_id: Option<String>,
    #[serde(default, deserialize_with = "Content::deserialize")]
    content: Option<Content>,
    #[serde(rename = "internal_chat_message_metadata_passthrough")]
    metadata: Option<TurnMetadata>,
}

#[derive(Debug, Default, Deserialize)]
struct TurnMetadata {
    turn_id: Option<String>,
}

impl TranscriptRecord {
    fn cwd(&self) -> Option<&str> {
        self.cwd
            .as_deref()
            .or_else(|| {
                self.payload
                    .as_ref()
                    .and_then(|payload| payload.cwd.as_deref())
            })
            .map(str::trim)
            .filter(|cwd| !cwd.is_empty())
    }

    /// The harness-native session id, from a `session_meta` record only.
    fn session_id(&self) -> Option<String> {
        if self.kind.as_deref() != Some("session_meta") {
            return None;
        }
        let payload = self.payload.as_ref()?;
        payload
            .id
            .as_deref()
            .or(payload.session_id.as_deref())
            .map(str::trim)
            .filter(|id| !id.is_empty())
            .map(str::to_owned)
    }

    fn is_visible_message(&self) -> bool {
        let Some(payload) = self.payload.as_ref() else {
            return false;
        };
        match self.kind.as_deref() {
            Some("event_msg") if payload.kind.as_deref() == Some("user_message") => payload
                .message
                .as_deref()
                .is_some_and(|text| !text.trim().is_empty()),
            Some("response_item") if payload.kind.as_deref() == Some("message") => {
                payload.role.as_deref() == Some("user")
                    || payload.role.as_deref() == Some("assistant")
            }
            _ => false,
        }
    }
}

/// Parses one rollout, streaming its records one at a time.
pub(crate) fn parse(path: &Path) -> Result<ParsedConversation, SkipReason> {
    let reader = open_reader(path)?;
    let fallback = file_time(path);
    let mut records = RecordReader::new(reader);
    let mut count = 0usize;
    let mut cwd: Option<PathBuf> = None;
    let mut session_id: Option<String> = None;
    let mut model: Option<String> = None;
    let mut last_seen: Option<DateTime<Utc>> = None;
    let mut history = History::new();
    let mut turn = TurnState::default();

    loop {
        let record = match records.next::<TranscriptRecord>() {
            Ok(Record::Parsed(record)) => record,
            Ok(Record::End) => break,
            Ok(Record::Blank | Record::Malformed) => continue,
            // A record too long to read cannot be kept whole; skipping the
            // conversation with a warning beats silently dropping its turn.
            Ok(Record::OverLimit) => return Err(SkipReason::TooMuchText),
            Err(_) => return Err(SkipReason::Unreadable),
        };
        count += 1;
        super::record_limit(count)?;
        if let Some(at) = parse_time(record.timestamp.as_deref()) {
            last_seen = Some(last_seen.map_or(at, |seen| seen.max(at)));
        }
        if let Some(value) = record.cwd() {
            cwd = Some(PathBuf::from(value));
        }
        // A fork rollout repeats `session_meta`; the first id names the
        // conversation, exactly as `peek` and the discovery scan read it.
        if session_id.is_none()
            && let Some(value) = record.session_id()
        {
            session_id = Some(value);
        }
        let kind = record.kind.as_deref().unwrap_or_default();
        let Some(payload) = record.payload.as_ref() else {
            continue;
        };
        let at = parse_time(record.timestamp.as_deref()).unwrap_or(fallback);
        match kind {
            "session_meta" => continue,
            "turn_context" => {
                if let Some(value) = payload.model.as_deref() {
                    let value = value.trim();
                    if !value.is_empty() {
                        model = Some(value.to_owned());
                    }
                }
                flush_turn(&mut history, &mut turn);
                turn.current_id = payload
                    .turn_id
                    .as_deref()
                    .map(str::trim)
                    .filter(|id| !id.is_empty())
                    .map(str::to_owned);
                continue;
            }
            "event_msg" => match payload.kind.as_deref() {
                Some("user_message") => {
                    let text = payload
                        .message
                        .as_deref()
                        .unwrap_or_default()
                        .trim()
                        .to_owned();
                    if text.is_empty() {
                        continue;
                    }
                    let turn_id = payload
                        .turn_id
                        .as_deref()
                        .map(str::trim)
                        .filter(|id| !id.is_empty())
                        .map(str::to_owned)
                        .or_else(|| turn.current_id.clone());
                    turn.push(
                        PendingUser {
                            text,
                            at,
                            canonical: true,
                            turn_id,
                        },
                        &mut history,
                    );
                }
                // Codex writes these boundaries, and a turn ends where it says.
                Some("task_started") => {
                    flush_turn(&mut history, &mut turn);
                    if let Some(id) = payload.turn_id.as_deref() {
                        turn.current_id = Some(id.trim().to_owned());
                    }
                }
                Some("task_complete" | "turn_aborted") => {
                    flush_turn(&mut history, &mut turn);
                    turn.current_id = None;
                }
                _ => {}
            },
            "response_item" => {
                if payload.kind.as_deref() != Some("message") {
                    continue;
                }
                let role = payload.role.as_deref().unwrap_or_default();
                let Some(content) = payload.content.as_ref() else {
                    continue;
                };
                let text = content.visible_text();
                if text.is_empty() {
                    continue;
                }
                match role {
                    "user" => {
                        // Only the response copy's own metadata may tie it to
                        // a canonical event; guessing the current turn would
                        // drop text an interrupted turn never duplicated.
                        let turn_id = payload
                            .metadata
                            .as_ref()
                            .and_then(|metadata| metadata.turn_id.as_deref())
                            .map(str::trim)
                            .filter(|id| !id.is_empty())
                            .map(str::to_owned);
                        turn.push(
                            PendingUser {
                                text,
                                at,
                                canonical: false,
                                turn_id,
                            },
                            &mut history,
                        );
                    }
                    "assistant" => {
                        flush_turn(&mut history, &mut turn);
                        history.push(KeptMessage {
                            role: MessageRole::Assistant,
                            text,
                            at,
                        });
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    }
    flush_turn(&mut history, &mut turn);

    let Some(cwd) = cwd else {
        return Err(SkipReason::NoWorkingDirectory);
    };
    let first_message_at = history.first_at();
    let first_user = history.first_text();
    let title = first_user
        .as_deref()
        .map(derive_codex_title)
        .filter(|title| !title.trim().is_empty())
        .unwrap_or_else(|| "Imported thread".to_owned());
    let messages = history.finish()?;
    if messages.is_empty() {
        return Err(SkipReason::Empty);
    }
    let created_at = first_message_at.unwrap_or(fallback);
    let last_active = messages
        .last()
        .map(|message| message.at)
        .or(last_seen)
        .unwrap_or(fallback);
    Ok(ParsedConversation {
        source: ImportSource::Codex,
        path: path.to_path_buf(),
        cwd,
        session_id,
        title,
        model,
        created_at,
        last_active,
        messages,
    })
}

/// The persisted state of the turn currently being read.
#[derive(Default)]
struct TurnState {
    /// Turn id the rollout last named.
    current_id: Option<String>,
    /// User records seen since the last boundary.
    pending: Vec<PendingUser>,
    /// Kept-text bytes waiting in `pending`.
    pending_bytes: usize,
    /// Set when this turn cannot be kept whole.
    ///
    /// Once set, nothing more is queued: the conversation is skipped with a
    /// warning, so holding the rest of the turn would only grow memory toward
    /// the file size.
    overflow: bool,
}

impl TurnState {
    /// Queues one user record, bounding the pending window as it grows.
    fn push(&mut self, user: PendingUser, history: &mut History) {
        if self.overflow {
            return;
        }
        self.pending_bytes = self
            .pending_bytes
            .saturating_add(user.text.len().saturating_add(16));
        self.pending.push(user);
        if self.pending_bytes > crate::import::MAX_KEPT_BYTES
            || self.pending.len() > crate::import::MAX_KEPT_MESSAGES
        {
            // A turn this large cannot be kept whole; skipping the conversation
            // with a warning is better than silently dropping its prompt. The
            // queued text is released now rather than kept until the turn ends.
            self.overflow = true;
            self.pending.clear();
            self.pending_bytes = 0;
            history.note_overflow();
        }
    }
}

/// One user turn waiting for its turn to end.
struct PendingUser {
    text: String,
    at: DateTime<Utc>,
    /// Whether it came from the canonical `event_msg` record.
    canonical: bool,
    /// Turn id the record names, when it names a valid one.
    turn_id: Option<String>,
}

/// Adds a turn's users to the history once its turn ends.
///
/// Two rules, in t3code's order. A canonical event's text that matches a
/// response copy with a valid turn id makes that whole turn canonical: every
/// response user record with the id is dropped, so generated setup context never
/// reaches the import. Then any response copy left that exactly repeats the
/// canonical text is dropped. A turn with no canonical event is response-only,
/// so its context stays; and a copy whose turn id names another turn is kept, so
/// a repeated prompt after an aborted turn loses neither turn's text.
fn flush_turn(history: &mut History, turn: &mut TurnState) {
    let pending = std::mem::take(&mut turn.pending);
    turn.pending_bytes = 0;
    // Canonical turn ids: a response copy whose text matches a canonical event,
    // tied by the id its own metadata carries.
    let canonical_ids: std::collections::HashSet<String> = pending
        .iter()
        .filter(|user| !user.canonical)
        .filter(|user| {
            user.turn_id.as_deref().is_some_and(|turn_id| {
                pending
                    .iter()
                    .any(|other| other.canonical && other.text == user.text)
                    && !turn_id.is_empty()
            })
        })
        .filter_map(|user| user.turn_id.clone())
        .collect();
    // How many response copies each canonical event may still claim by text.
    let mut canonical_texts: HashMap<String, usize> = HashMap::new();
    for user in pending.iter().filter(|user| user.canonical) {
        *canonical_texts.entry(user.text.clone()).or_default() += 1;
    }
    for user in pending {
        if user.canonical {
            history.push(KeptMessage {
                role: MessageRole::User,
                text: user.text,
                at: user.at,
            });
            continue;
        }
        if user
            .turn_id
            .as_deref()
            .is_some_and(|turn_id| canonical_ids.contains(turn_id))
        {
            continue;
        }
        if let Some(left) = canonical_texts.get_mut(&user.text)
            && *left > 0
        {
            *left -= 1;
            continue;
        }
        history.push(KeptMessage {
            role: MessageRole::User,
            text: user.text,
            at: user.at,
        });
    }
}

/// Drops leading generated setup context before deriving a title.
///
/// The messages themselves keep it: only the list title reads the request.
fn derive_codex_title(text: &str) -> String {
    let mut rest = text.trim_start();
    loop {
        if let Some(after) = strip_setup_block(rest) {
            rest = after.trim_start();
            continue;
        }
        if let Some(after) = rest.strip_prefix("# AGENTS.md instructions") {
            rest = after
                .split_once('\n')
                .map(|(_, remainder)| remainder)
                .unwrap_or_default();
            continue;
        }
        if let Some(after) = rest.strip_prefix("## My request for Codex:") {
            rest = after;
            continue;
        }
        break;
    }
    derive_title(rest)
}

fn strip_setup_block(text: &str) -> Option<&str> {
    let rest = text.strip_prefix('<')?;
    let name_end = rest
        .find(|character: char| character.is_whitespace() || character == '>')
        .unwrap_or(rest.len());
    let name = &rest[..name_end];
    if !matches!(
        name,
        "recommended_plugins"
            | "environment_context"
            | "user_instructions"
            | "INSTRUCTIONS"
            | "in-app-browser-context"
            | "external_codex_apps_open_page"
    ) {
        return None;
    }
    let close = format!("</{name}>");
    let end = rest.find(&close)?;
    Some(&rest[end + close.len()..])
}

fn file_time(path: &Path) -> DateTime<Utc> {
    std::fs::metadata(path)
        .and_then(|metadata| metadata.modified())
        .map(DateTime::<Utc>::from)
        .unwrap_or_else(|_| Utc::now())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn write_rollout(lines: &[serde_json::Value]) -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().expect("temp dir");
        let directory = dir.path().join("sessions/2026/11/20");
        std::fs::create_dir_all(&directory).expect("sessions dir");
        let path = directory.join("rollout-2026-11-20T10-00-00-session.jsonl");
        let mut body = String::new();
        for line in lines {
            body.push_str(&line.to_string());
            body.push('\n');
        }
        std::fs::write(&path, body).expect("rollout");
        (dir, path)
    }

    fn meta() -> serde_json::Value {
        json!({
            "timestamp": "2026-11-20T10:00:00Z",
            "type": "session_meta",
            "payload": {"id": "codex-session-1", "cwd": "/home/user/code/api"},
        })
    }

    fn event_user(text: &str) -> serde_json::Value {
        json!({
            "timestamp": "2026-11-20T10:00:01Z",
            "type": "event_msg",
            "payload": {"type": "user_message", "message": text},
        })
    }

    fn response_user(text: &str, turn_id: Option<&str>) -> serde_json::Value {
        let mut payload = json!({
            "type": "message",
            "role": "user",
            "content": [{"type": "input_text", "text": text}],
        });
        if let Some(turn_id) = turn_id {
            payload["internal_chat_message_metadata_passthrough"] = json!({"turn_id": turn_id});
        }
        json!({
            "timestamp": "2026-11-20T10:00:01Z",
            "type": "response_item",
            "payload": payload,
        })
    }

    fn assistant(text: &str) -> serde_json::Value {
        json!({
            "timestamp": "2026-11-20T10:00:02Z",
            "type": "response_item",
            "payload": {
                "type": "message",
                "role": "assistant",
                "content": [{"type": "output_text", "text": text}],
            },
        })
    }

    fn turn_context(turn_id: &str) -> serde_json::Value {
        json!({
            "timestamp": "2026-11-20T10:00:00Z",
            "type": "turn_context",
            "payload": {"turn_id": turn_id, "model": "gpt-5.1-codex"},
        })
    }

    fn aborted(turn_id: &str) -> serde_json::Value {
        json!({
            "timestamp": "2026-11-20T10:00:03Z",
            "type": "event_msg",
            "payload": {"type": "turn_aborted", "turn_id": turn_id, "reason": "interrupted"},
        })
    }

    fn user_texts(parsed: &ParsedConversation) -> Vec<&str> {
        parsed
            .messages
            .iter()
            .filter(|message| message.role == MessageRole::User)
            .map(|message| message.text.as_str())
            .collect()
    }

    #[test]
    fn a_canonical_event_drops_its_matching_response_copy() {
        let (_dir, path) = write_rollout(&[
            meta(),
            turn_context("turn-1"),
            event_user("do the thing"),
            response_user("do the thing", Some("turn-1")),
            assistant("did it"),
        ]);
        let parsed = parse(&path).expect("parse");
        let texts: Vec<&str> = parsed
            .messages
            .iter()
            .map(|message| message.text.as_str())
            .collect();
        assert_eq!(texts, ["do the thing", "did it"]);
        assert_eq!(parsed.session_id.as_deref(), Some("codex-session-1"));
    }

    #[test]
    fn a_repeated_prompt_after_an_aborted_turn_keeps_the_aborted_turn() {
        // Turn 1 was interrupted after only its response copy was written; turn
        // 2 repeats the prompt canonically. The copies belong to different
        // turns, so neither turn's text may be dropped.
        let (_dir, path) = write_rollout(&[
            meta(),
            turn_context("turn-1"),
            response_user("do the thing", Some("turn-1")),
            aborted("turn-1"),
            turn_context("turn-2"),
            event_user("do the thing"),
            response_user("do the thing", Some("turn-2")),
            assistant("did it"),
        ]);
        let parsed = parse(&path).expect("parse");
        assert_eq!(user_texts(&parsed), ["do the thing", "do the thing"]);
        assert_eq!(parsed.messages.last().expect("last").text, "did it");
    }

    #[test]
    fn a_response_copy_without_a_turn_id_is_dropped_by_its_text() {
        // The legacy shape: the copy names no turn, so only the canonical
        // event's text can identify it.
        let (_dir, path) = write_rollout(&[
            meta(),
            turn_context("turn-1"),
            event_user("do the thing"),
            response_user("do the thing", None),
            assistant("did it"),
        ]);
        let parsed = parse(&path).expect("parse");
        assert_eq!(user_texts(&parsed), ["do the thing"]);
    }

    #[test]
    fn a_legacy_metadata_less_copy_before_the_event_is_dropped() {
        let (_dir, path) = write_rollout(&[
            meta(),
            response_user("do the thing", None),
            event_user("do the thing"),
            assistant("did it"),
        ]);
        let parsed = parse(&path).expect("parse");
        assert_eq!(user_texts(&parsed), ["do the thing"]);
    }

    #[test]
    fn the_canonical_event_wins_over_its_generated_context() {
        // t3code's fixture: the environment block, the project's private
        // AGENTS.md instructions and the prompt are all copies of one turn.
        let (_dir, path) = write_rollout(&[
            meta(),
            response_user(
                "<environment_context>\n<cwd>/tmp/project</cwd>\n<shell>zsh</shell>\n</environment_context>",
                Some("turn-1"),
            ),
            response_user(
                "# AGENTS.md instructions for /tmp/project\n\n<INSTRUCTIONS>\nPrivate project rules\n</INSTRUCTIONS>",
                Some("turn-1"),
            ),
            event_user("Do something here so it looks like a real project."),
            response_user(
                "Do something here so it looks like a real project.",
                Some("turn-1"),
            ),
            assistant("Created the project."),
        ]);
        let parsed = parse(&path).expect("parse");
        let texts: Vec<&str> = parsed
            .messages
            .iter()
            .map(|message| message.text.as_str())
            .collect();
        assert_eq!(
            texts,
            [
                "Do something here so it looks like a real project.",
                "Created the project."
            ]
        );
        assert_eq!(
            parsed.title,
            "Do something here so it looks like a real project."
        );
        assert!(
            !texts
                .iter()
                .any(|text| text.contains("Private project rules")),
            "the private instructions never reach the import"
        );
    }

    #[test]
    fn response_only_turns_keep_their_generated_context() {
        let context = "<environment_context>\n<cwd>/tmp/project</cwd>\n</environment_context>";
        let (_dir, path) = write_rollout(&[
            meta(),
            response_user(context, None),
            response_user("Initialize Git and add a README.", None),
            assistant("done"),
        ]);
        let parsed = parse(&path).expect("parse");
        assert_eq!(
            user_texts(&parsed),
            [context, "Initialize Git and add a README."]
        );
    }

    #[test]
    fn mixed_formats_keep_turns_whose_ids_repeat_after_an_assistant() {
        // t3code's fixture: the older turn and the repeated prompt after the
        // reply are separate turns, so neither is dropped.
        let (_dir, path) = write_rollout(&[
            meta(),
            response_user("Keep this older prompt", Some("turn-older")),
            event_user("Keep this newer prompt"),
            response_user("Keep this newer prompt", Some("turn-newer")),
            assistant("Ask again when needed"),
            response_user("Keep this newer prompt", Some("turn-newer")),
        ]);
        let parsed = parse(&path).expect("parse");
        let texts: Vec<&str> = parsed
            .messages
            .iter()
            .map(|message| message.text.as_str())
            .collect();
        assert_eq!(
            texts,
            [
                "Keep this older prompt",
                "Keep this newer prompt",
                "Ask again when needed",
                "Keep this newer prompt"
            ]
        );
    }

    #[test]
    fn a_copy_with_another_turns_id_is_kept() {
        let (_dir, path) = write_rollout(&[
            meta(),
            turn_context("turn-1"),
            response_user("do the thing", Some("turn-1")),
            assistant("stale reply"),
            turn_context("turn-2"),
            event_user("do the thing"),
            response_user("do the thing", Some("turn-2")),
            assistant("did it"),
        ]);
        let parsed = parse(&path).expect("parse");
        assert_eq!(user_texts(&parsed), ["do the thing", "do the thing"]);
    }

    #[test]
    fn setup_context_is_kept_in_messages_but_not_in_the_title() {
        let (_dir, path) = write_rollout(&[
            meta(),
            event_user(
                "<environment_context>\nlinux\n</environment_context>\n## My request for Codex:\nFix the flaky test",
            ),
            assistant("fixed"),
        ]);
        let parsed = parse(&path).expect("parse");
        assert_eq!(parsed.title, "Fix the flaky test");
        assert!(parsed.messages[0].text.contains("<environment_context>"));
    }

    #[test]
    fn malformed_lines_are_ignored_and_empty_conversations_are_skipped() {
        let dir = tempfile::tempdir().expect("temp dir");
        let directory = dir.path().join("sessions/2026/11/20");
        std::fs::create_dir_all(&directory).expect("dir");
        let empty = directory.join("rollout-empty.jsonl");
        std::fs::write(&empty, format!("{}\nnot json\n", meta())).expect("empty");
        assert_eq!(parse(&empty).err(), Some(SkipReason::Empty));

        let mixed = directory.join("rollout-mixed.jsonl");
        let body = format!(
            "{}\n{}\nbroken\n{}\n",
            meta(),
            event_user("real"),
            assistant("reply")
        );
        std::fs::write(&mixed, body).expect("mixed");
        let parsed = parse(&mixed).expect("parse");
        assert_eq!(parsed.messages.len(), 2);
    }

    #[test]
    fn a_streamed_turn_keeps_its_role_order() {
        let (_dir, path) = write_rollout(&[
            meta(),
            turn_context("turn-1"),
            event_user("first"),
            assistant("answer one"),
            turn_context("turn-2"),
            event_user("second"),
            assistant("answer two"),
        ]);
        let parsed = parse(&path).expect("parse");
        assert_eq!(parsed.model.as_deref(), Some("gpt-5.1-codex"));
        let roles: Vec<MessageRole> = parsed.messages.iter().map(|message| message.role).collect();
        assert_eq!(
            roles,
            [
                MessageRole::User,
                MessageRole::Assistant,
                MessageRole::User,
                MessageRole::Assistant
            ]
        );
    }

    #[test]
    fn a_forked_rollout_resumes_the_first_session() {
        // Only the first `session_meta` names the conversation; a fork repeats
        // the record with another id.
        let (_dir, path) = write_rollout(&[
            meta(),
            json!({
                "timestamp": "2026-11-20T10:05:00Z",
                "type": "session_meta",
                "payload": {"id": "codex-fork-2", "cwd": "/home/user/code/api"},
            }),
            event_user("carry on"),
            assistant("carried on"),
        ]);
        let parsed = parse(&path).expect("parse");
        assert_eq!(parsed.session_id.as_deref(), Some("codex-session-1"));
        let peek = peek(&path).expect("peek");
        assert_eq!(peek.session_id.as_deref(), Some("codex-session-1"));
    }

    #[test]
    fn the_first_message_names_the_start_whether_user_or_assistant() {
        let (_dir, path) = write_rollout(&[
            meta(),
            json!({
                "timestamp": "2026-11-20T10:00:00Z",
                "type": "turn_context",
                "payload": {"model": "gpt-5.1-codex"},
            }),
            json!({
                "timestamp": "2026-11-20T10:00:07Z",
                "type": "event_msg",
                "payload": {"type": "user_message", "message": "only a prompt"},
            }),
        ]);
        let parsed = parse(&path).expect("parse");
        assert_eq!(parsed.created_at.to_rfc3339(), "2026-11-20T10:00:07+00:00");
    }

    #[test]
    fn a_prompt_sharing_a_record_with_a_large_omitted_field_is_kept() {
        let (_dir, path) = write_rollout(&[
            meta(),
            turn_context("turn-1"),
            event_user("summarise this output"),
            json!({
                "timestamp": "2026-11-20T10:00:02Z",
                "type": "response_item",
                "payload": {
                    "type": "message",
                    "role": "user",
                    "content": [{"type": "input_text", "text": "summarise this output"}],
                    "attachments": [{"data": "A".repeat(5 * 1024 * 1024)}],
                    "internal_chat_message_metadata_passthrough": {"turn_id": "turn-1"},
                },
            }),
            assistant("summarised"),
        ]);
        let parsed = parse(&path).expect("parse");
        assert_eq!(user_texts(&parsed), ["summarise this output"]);
    }

    #[test]
    fn a_pending_turn_over_the_kept_text_limit_skips_the_conversation() {
        let mut lines = vec![meta(), turn_context("turn-1")];
        // Many canonical user records in one turn, none answered.
        for _ in 0..3 {
            lines.push(event_user(&"y".repeat(12 * 1024 * 1024)));
        }
        let (_dir, path) = write_rollout(&lines);
        assert_eq!(parse(&path).err(), Some(SkipReason::TooMuchText));
    }

    #[test]
    fn an_overflowing_pending_turn_releases_its_text_at_once() {
        let mut history = History::new();
        let mut turn = TurnState::default();
        for _ in 0..3 {
            turn.push(
                PendingUser {
                    text: "y".repeat(12 * 1024 * 1024),
                    at: Utc::now(),
                    canonical: true,
                    turn_id: None,
                },
                &mut history,
            );
        }
        assert!(turn.pending.is_empty(), "the pending window was released");
        assert_eq!(turn.pending_bytes, 0);
        assert_eq!(history.retained_bytes(), 0);
        // Anything queued after the overflow is not retained either.
        turn.push(
            PendingUser {
                text: "z".repeat(12 * 1024 * 1024),
                at: Utc::now(),
                canonical: true,
                turn_id: None,
            },
            &mut history,
        );
        assert!(turn.pending.is_empty());
        assert_eq!(history.finish().err(), Some(SkipReason::TooMuchText));
    }
}
