//! Codex rollouts: `<CODEX_HOME>/sessions/YYYY/MM/DD/rollout-*.jsonl`.
//!
//! Records are a mix of `session_meta`, `turn_context`, canonical `event_msg`
//! user turns and `response_item` messages. Codex can write the same user turn
//! twice: once as the canonical event and once as a response item that may also
//! carry generated setup context. Ambiguous copies are kept; a response copy is
//! dropped only when a canonical event and a valid shared turn id identify the
//! same user turn.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use h4rn_protocol::{ImportSource, MessageRole};
use serde_json::Value;

use super::{
    History, KeptMessage, ParsedConversation, Peek, SkipReason, derive_title, extract_content_text,
    open_reader, parse_time, read_line_bounded, record_limit,
};

/// Reads just enough of a rollout to group it by folder.
pub(crate) fn peek(path: &Path) -> Option<Peek> {
    let mut reader = open_reader(path).ok()?;
    let mut line = Vec::new();
    let mut records = 0;
    while let Ok(Some(complete)) = read_line_bounded(&mut reader, &mut line) {
        records += 1;
        if records > 40 {
            break;
        }
        if !complete {
            continue;
        }
        let Ok(record) = serde_json::from_slice::<Value>(&line) else {
            continue;
        };
        if let Some(cwd) = record_cwd(&record) {
            return Some(Peek {
                cwd: PathBuf::from(cwd),
                session_id: record_session_id(&record),
            });
        }
    }
    None
}

/// Parses one rollout, streaming its records one at a time.
pub(crate) fn parse(path: &Path) -> Result<ParsedConversation, SkipReason> {
    let mut reader = open_reader(path)?;
    let fallback = file_time(path);
    let mut line = Vec::new();
    let mut records = 0usize;
    let mut cwd: Option<PathBuf> = None;
    let mut session_id: Option<String> = None;
    let mut model: Option<String> = None;
    let mut last_seen: Option<DateTime<Utc>> = None;
    let mut history = History::new();
    let mut first_message_at: Option<DateTime<Utc>> = None;
    let mut turn_users: Vec<PendingUser> = Vec::new();
    let mut canonical_texts: HashSet<String> = HashSet::new();

    while let Ok(Some(complete)) = read_line_bounded(&mut reader, &mut line) {
        records += 1;
        record_limit(records)?;
        if !complete {
            continue;
        }
        let Ok(record) = serde_json::from_slice::<Value>(&line) else {
            continue;
        };
        if let Some(at) = parse_time(record.get("timestamp")) {
            last_seen = Some(last_seen.map_or(at, |seen| seen.max(at)));
        }
        if let Some(value) = record_cwd(&record) {
            cwd = Some(PathBuf::from(value));
        }
        if let Some(value) = record_session_id(&record) {
            session_id = Some(value);
        }
        let kind = record
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let payload = record.get("payload").unwrap_or(&Value::Null);
        match kind {
            "session_meta" => continue,
            "turn_context" => {
                if let Some(value) = payload.get("model").and_then(Value::as_str) {
                    let value = value.trim();
                    if !value.is_empty() {
                        model = Some(value.to_owned());
                    }
                }
                continue;
            }
            "event_msg" if payload.get("type").and_then(Value::as_str) == Some("user_message") => {
                let text = payload
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .trim()
                    .to_owned();
                if text.is_empty() {
                    continue;
                }
                let at = parse_time(record.get("timestamp")).unwrap_or(fallback);
                canonical_texts.insert(text.clone());
                turn_users.push(PendingUser {
                    text,
                    at,
                    canonical: true,
                    turn_id: None,
                });
                continue;
            }
            "response_item" => {}
            _ => continue,
        }
        if payload.get("type").and_then(Value::as_str) != Some("message") {
            continue;
        }
        let role = payload
            .get("role")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let text = extract_content_text(payload.get("content").unwrap_or(&Value::Null));
        if text.is_empty() {
            continue;
        }
        let at = parse_time(record.get("timestamp")).unwrap_or(fallback);
        match role {
            "user" => {
                let turn_id = payload
                    .get("internal_chat_message_metadata_passthrough")
                    .and_then(Value::as_object)
                    .and_then(|metadata| metadata.get("turn_id"))
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|id| !id.is_empty())
                    .map(str::to_owned);
                turn_users.push(PendingUser {
                    text,
                    at,
                    canonical: false,
                    turn_id,
                });
            }
            "assistant" => {
                flush_turn(&mut history, &mut turn_users, &mut canonical_texts);
                first_message_at.get_or_insert(at);
                history.push(KeptMessage {
                    role: MessageRole::Assistant,
                    text,
                    at,
                });
            }
            _ => {}
        }
    }
    flush_turn(&mut history, &mut turn_users, &mut canonical_texts);

    let Some(cwd) = cwd else {
        return Err(SkipReason::NoWorkingDirectory);
    };
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

/// One user turn waiting for its turn to end.
struct PendingUser {
    text: String,
    at: DateTime<Utc>,
    /// Whether it came from the canonical `event_msg` record.
    canonical: bool,
    /// Shared turn id, when the record names one.
    turn_id: Option<String>,
}

/// Adds a turn's users to the history once its assistant reply begins.
///
/// A response copy is dropped only when its text matches a canonical event and
/// its valid turn id is one the canonical event identifies as the same turn.
/// Everything ambiguous stays, so no user text is lost.
fn flush_turn(
    history: &mut History,
    users: &mut Vec<PendingUser>,
    canonical_texts: &mut HashSet<String>,
) {
    let dropped_ids: HashSet<String> = users
        .iter()
        .filter(|user| !user.canonical)
        .filter(|user| canonical_texts.contains(&user.text))
        .filter_map(|user| user.turn_id.clone())
        .collect();
    for user in users.drain(..) {
        if !user.canonical
            && let Some(turn_id) = &user.turn_id
            && dropped_ids.contains(turn_id)
        {
            continue;
        }
        history.push(KeptMessage {
            role: MessageRole::User,
            text: user.text,
            at: user.at,
        });
    }
    canonical_texts.clear();
}

fn record_cwd(record: &Value) -> Option<String> {
    let payload = record.get("payload");
    record
        .get("cwd")
        .and_then(Value::as_str)
        .or_else(|| {
            payload
                .and_then(|payload| payload.get("cwd"))
                .and_then(Value::as_str)
        })
        .map(str::trim)
        .filter(|cwd| !cwd.is_empty())
        .map(str::to_owned)
}

fn record_session_id(record: &Value) -> Option<String> {
    if record.get("type").and_then(Value::as_str) != Some("session_meta") {
        return None;
    }
    let payload = record.get("payload")?;
    payload
        .get("id")
        .and_then(Value::as_str)
        .or_else(|| payload.get("session_id").and_then(Value::as_str))
        .map(str::trim)
        .filter(|id| !id.is_empty())
        .map(str::to_owned)
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

    fn write_rollout(lines: &[Value]) -> (tempfile::TempDir, PathBuf) {
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

    fn meta() -> Value {
        json!({
            "timestamp": "2026-11-20T10:00:00Z",
            "type": "session_meta",
            "payload": {"id": "codex-session-1", "cwd": "/home/user/code/api"},
        })
    }

    fn event_user(text: &str) -> Value {
        json!({
            "timestamp": "2026-11-20T10:00:01Z",
            "type": "event_msg",
            "payload": {"type": "user_message", "message": text},
        })
    }

    fn response_user(text: &str, turn_id: Option<&str>) -> Value {
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

    fn assistant(text: &str) -> Value {
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

    #[test]
    fn a_canonical_event_drops_its_matching_response_copy() {
        let (_dir, path) = write_rollout(&[
            meta(),
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
    fn ambiguous_context_stays_when_no_turn_id_matches() {
        let (_dir, path) = write_rollout(&[
            meta(),
            event_user("do the thing"),
            response_user("do the thing", None),
            assistant("did it"),
        ]);
        let parsed = parse(&path).expect("parse");
        let users: Vec<&str> = parsed
            .messages
            .iter()
            .filter(|message| message.role == MessageRole::User)
            .map(|message| message.text.as_str())
            .collect();
        assert_eq!(users, ["do the thing", "do the thing"]);
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
            json!({
                "timestamp": "2026-11-20T10:00:00Z",
                "type": "turn_context",
                "payload": {"model": "gpt-5.1-codex"},
            }),
            event_user("first"),
            assistant("answer one"),
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
}
