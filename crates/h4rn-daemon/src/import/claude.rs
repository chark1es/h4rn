//! Claude Code transcripts: `~/.claude/projects/<project>/<session>.jsonl`.
//!
//! Each line is one record. User and assistant messages carry a `message`
//! whose content blocks hold the visible text; tool activity, side chains and
//! Claude Code's own UI output are left out. Slash commands and `!` shell
//! commands are kept exactly as the person typed them.

use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use h4rn_protocol::{ImportSource, MessageRole};
use serde_json::Value;

use super::{
    History, KeptMessage, ParsedConversation, Peek, SkipReason, derive_title, extract_content_text,
    open_reader, parse_time, read_line_bounded, record_limit,
};

/// Reads just enough of a transcript to group it by folder.
pub(crate) fn peek(path: &Path) -> Option<Peek> {
    let mut reader = open_reader(path).ok()?;
    let mut line = Vec::new();
    let mut records = 0;
    while let Ok(Some(complete)) = read_line_bounded(&mut reader, &mut line) {
        records += 1;
        if records > 200 {
            break;
        }
        if !complete {
            continue;
        }
        let Ok(record) = serde_json::from_slice::<Value>(&line) else {
            continue;
        };
        let cwd = record
            .get("cwd")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|cwd| !cwd.is_empty());
        let session = record
            .get("sessionId")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|session| !session.is_empty())
            .map(str::to_owned)
            .or_else(|| file_session(path));
        if let Some(cwd) = cwd {
            return Some(Peek {
                cwd: PathBuf::from(cwd),
                session_id: session,
            });
        }
    }
    None
}

/// Parses one conversation, streaming its records one at a time.
pub(crate) fn parse(path: &Path) -> Result<ParsedConversation, SkipReason> {
    let mut reader = open_reader(path)?;
    let fallback = file_time(path);
    let mut line = Vec::new();
    let mut records = 0usize;
    let mut cwd: Option<PathBuf> = None;
    let mut session_id = file_session(path);
    let mut model: Option<String> = None;
    let mut custom_title: Option<Option<String>> = None;
    let mut ai_title: Option<String> = None;
    let mut last_seen: Option<DateTime<Utc>> = None;
    let mut history = History::new();
    let mut first_message_at: Option<DateTime<Utc>> = None;
    let mut has_real_reply = false;

    while let Ok(Some(complete)) = read_line_bounded(&mut reader, &mut line) {
        records += 1;
        record_limit(records)?;
        if !complete {
            continue;
        }
        let Ok(record) = serde_json::from_slice::<Value>(&line) else {
            continue;
        };
        if let Some(value) = record
            .get("cwd")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
        {
            cwd = Some(PathBuf::from(value));
        }
        if let Some(value) = record
            .get("sessionId")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
        {
            session_id = Some(value.to_owned());
        }
        if let Some(value) = record.get("aiTitle").and_then(Value::as_str) {
            let value = value.trim();
            if !value.is_empty() {
                ai_title = Some(claude_title(value));
            }
        }
        if let Some(value) = record.get("customTitle").and_then(Value::as_str) {
            let value = value.trim();
            // Claude flushes the AI title after a rename; an empty rename clears
            // the custom title.
            custom_title = Some(if value.is_empty() {
                None
            } else {
                let title = claude_title(value);
                (!title.is_empty()).then_some(title)
            });
        }
        if let Some(at) = parse_time(record.get("timestamp")) {
            last_seen = Some(last_seen.map_or(at, |seen| seen.max(at)));
        }
        if is_hidden(&record) {
            continue;
        }
        let kind = record
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if kind != "user" && kind != "assistant" {
            continue;
        }
        let Some(message) = record.get("message") else {
            continue;
        };
        let text = extract_content_text(message.get("content").unwrap_or(&Value::Null));
        if text.is_empty() {
            continue;
        }
        let at = parse_time(record.get("timestamp")).unwrap_or(fallback);
        let (role, text) = if kind == "user" {
            let Some(text) = claude_user_text(&text) else {
                continue;
            };
            (MessageRole::User, text)
        } else {
            let recorded = message
                .get("model")
                .and_then(Value::as_str)
                .map(str::trim)
                .unwrap_or_default();
            if !recorded.is_empty() && recorded != SYNTHETIC_MODEL {
                model = Some(recorded.to_owned());
                has_real_reply = true;
            }
            (MessageRole::Assistant, text)
        };
        first_message_at.get_or_insert(at);
        history.push(KeptMessage { role, text, at });
    }

    let Some(cwd) = cwd else {
        return Err(SkipReason::NoWorkingDirectory);
    };
    if !has_real_reply {
        return Err(SkipReason::OnlyCommands);
    }
    let first_user = history.first_text();
    let title = custom_title
        .flatten()
        .or(ai_title)
        .or_else(|| first_user.as_deref().map(derive_title))
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
        source: ImportSource::ClaudeCode,
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

const SYNTHETIC_MODEL: &str = "<synthetic>";

fn is_hidden(record: &Value) -> bool {
    record.get("isSidechain").and_then(Value::as_bool) == Some(true)
        || record.get("isMeta").and_then(Value::as_bool) == Some(true)
        || record.get("isCompactSummary").and_then(Value::as_bool) == Some(true)
}

/// What a user turn shows, or `None` for Claude Code's own UI output.
///
/// Slash commands and `!` shell commands are kept as the person typed them; the
/// command's output, background task notifications and interruption markers are
/// left out.
fn claude_user_text(text: &str) -> Option<String> {
    let trimmed = text.trim();
    if trimmed.starts_with("<task-notification")
        || trimmed.starts_with("<local-command-stdout")
        || trimmed.starts_with("<local-command-stderr")
        || trimmed.starts_with("<local-command-caveat")
        || trimmed.starts_with("<bash-stdout")
        || trimmed.starts_with("<bash-stderr")
        || trimmed == "[Request interrupted by user]"
        || trimmed == "[Request interrupted by user for tool use]"
    {
        return None;
    }
    if let Some(inner) = tag_body(trimmed, "bash-input") {
        return Some(format!("!{inner}"));
    }
    if trimmed.starts_with("<command-name>") || trimmed.starts_with("<command-message>") {
        let name = tag_body(trimmed, "command-name")
            .map(str::to_owned)
            .or_else(|| tag_body(trimmed, "command-message").map(|name| format!("/{name}")))
            .unwrap_or_default();
        let args = tag_body(trimmed, "command-args").unwrap_or_default();
        if name.trim().is_empty() {
            return None;
        }
        return Some(if args.trim().is_empty() {
            name
        } else {
            format!("{} {}", name.trim(), args.trim())
        });
    }
    Some(trimmed.to_owned())
}

fn tag_body<'a>(text: &'a str, tag: &str) -> Option<&'a str> {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let start = text.find(&open)? + open.len();
    let rest = &text[start..];
    let end = rest.find(&close)?;
    Some(rest[..end].trim())
}

/// A saved title that is really a user turn's markup is not shown as a title.
fn claude_title(value: &str) -> String {
    let trimmed = value.trim();
    if claude_user_text(trimmed).as_deref() == Some(trimmed) {
        trimmed.to_owned()
    } else {
        String::new()
    }
}

fn file_session(path: &Path) -> Option<String> {
    let stem = path.file_stem()?.to_string_lossy();
    let stem = stem.trim();
    (!stem.is_empty()).then(|| stem.to_owned())
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

    fn write_transcript(lines: &[Value]) -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("projects").join("-home-user-code-api");
        std::fs::create_dir_all(&path).expect("project dir");
        let path = path.join("session-1.jsonl");
        let mut body = String::new();
        for line in lines {
            body.push_str(&line.to_string());
            body.push('\n');
        }
        std::fs::write(&path, body).expect("transcript");
        (dir, path)
    }

    fn record(kind: &str, text: &str, model: Option<&str>) -> Value {
        let mut value = json!({
            "type": kind,
            "sessionId": "session-1",
            "cwd": "/home/user/code/api",
            "timestamp": "2026-11-20T10:00:00Z",
            "message": {
                "role": kind,
                "content": [{"type": "text", "text": text}],
            },
        });
        if let Some(model) = model {
            value["message"]["model"] = json!(model);
        }
        value
    }

    #[test]
    fn slash_and_shell_commands_are_kept_as_typed_and_their_output_is_not() {
        let (_dir, path) = write_transcript(&[
            record(
                "user",
                "<command-name>/review</command-name><command-message>review</command-message><command-args>src/</command-args>",
                None,
            ),
            record("assistant", "reviewed", Some("claude-sonnet-5")),
            record(
                "user",
                "<local-command-stdout>lots of output</local-command-stdout>",
                None,
            ),
            record("user", "[Request interrupted by user]", None),
            record("user", "<bash-input>ls</bash-input>", None),
            record("assistant", "listed", Some("claude-sonnet-5")),
        ]);
        let parsed = parse(&path).expect("parse");
        let texts: Vec<&str> = parsed
            .messages
            .iter()
            .map(|message| message.text.as_str())
            .collect();
        assert_eq!(texts, ["/review src/", "reviewed", "!ls", "listed"]);
        assert_eq!(parsed.model.as_deref(), Some("claude-sonnet-5"));
        assert_eq!(parsed.session_id.as_deref(), Some("session-1"));
    }

    #[test]
    fn a_conversation_with_only_commands_and_synthetic_errors_is_skipped() {
        let (_dir, path) = write_transcript(&[
            record("user", "<command-name>/clear</command-name>", None),
            record("assistant", "Invalid API key", Some("<synthetic>")),
        ]);
        assert_eq!(parse(&path).err(), Some(SkipReason::OnlyCommands));
    }

    #[test]
    fn malformed_lines_are_ignored_and_side_chains_are_hidden() {
        let dir = tempfile::tempdir().expect("temp dir");
        let directory = dir.path().join("projects/p");
        std::fs::create_dir_all(&directory).expect("dir");
        let path = directory.join("s.jsonl");
        let mut side = record("assistant", "hidden side chain", Some("claude-sonnet-5"));
        side["isSidechain"] = json!(true);
        let body = format!(
            "{}\n{}\nnot json at all\n{}\n",
            record("user", "real request", None),
            side,
            record("assistant", "real reply", Some("claude-sonnet-5"))
        );
        std::fs::write(&path, body).expect("write");
        let parsed = parse(&path).expect("parse");
        let texts: Vec<&str> = parsed
            .messages
            .iter()
            .map(|message| message.text.as_str())
            .collect();
        assert_eq!(texts, ["real request", "real reply"]);
    }

    #[test]
    fn an_ai_title_wins_and_a_command_only_title_falls_back_to_the_request() {
        let (_dir, path) = write_transcript(&[
            record("user", "Add a health check", None),
            record("assistant", "done", Some("claude-sonnet-5")),
        ]);
        let parsed = parse(&path).expect("parse");
        assert_eq!(parsed.title, "Add a health check");
    }

    #[test]
    fn the_record_limit_is_a_skip_reason() {
        assert!(crate::import::record_limit(crate::import::MAX_RECORDS).is_ok());
        assert_eq!(
            crate::import::record_limit(crate::import::MAX_RECORDS + 1),
            Err(SkipReason::TooManyRecords)
        );
    }
}
