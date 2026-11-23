//! Claude Code transcripts: `~/.claude/projects/<project>/<session>.jsonl`.
//!
//! Each line is one record. User and assistant messages carry a `message`
//! whose content blocks hold the visible text; tool activity, side chains and
//! Claude Code's own UI output are left out. Slash commands and `!` shell
//! commands are kept exactly as the person typed them.
//!
//! Claude Code records its own UI as ordinary user turns: notification and
//! command-output elements, plus `<command-…>` and `<bash-input>` markup. A
//! block is treated as that markup only when the element is complete, so a
//! prompt that merely begins with `<task-notification` stays a prompt.

use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use h4rn_protocol::{ImportSource, MessageRole};
use serde::Deserialize;

use super::record::{Content, RawText, Record, RecordReader};
use super::{
    History, KeptMessage, ParsedConversation, Peek, SkipReason, derive_title, open_reader,
    parse_time, read_line_bounded,
};

/// Reads just enough of a transcript to group it by folder.
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
            && let Some(value) = record
                .cwd
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty())
        {
            cwd = Some(PathBuf::from(value));
        }
        if session_id.is_none() {
            session_id = record
                .session_id
                .as_deref()
                .map(str::trim)
                .filter(|session| !session.is_empty())
                .map(str::to_owned);
        }
        if !has_visible && record.is_visible_message() {
            has_visible = true;
        }
    }
    let cwd = cwd?;
    Some(Peek {
        cwd,
        session_id: session_id.or_else(|| file_session(path)),
        has_visible,
    })
}

/// One JSONL record, with only the fields the import reads.
#[derive(Debug, Default, Deserialize)]
struct TranscriptRecord {
    #[serde(rename = "type")]
    kind: Option<String>,
    #[serde(rename = "sessionId")]
    session_id: Option<String>,
    cwd: Option<String>,
    timestamp: Option<String>,
    #[serde(rename = "aiTitle")]
    ai_title: Option<String>,
    #[serde(rename = "customTitle")]
    custom_title: Option<String>,
    #[serde(rename = "isSidechain")]
    is_sidechain: Option<bool>,
    #[serde(rename = "isMeta")]
    is_meta: Option<bool>,
    #[serde(rename = "isCompactSummary")]
    is_compact_summary: Option<bool>,
    message: Option<TranscriptMessage>,
}

#[derive(Debug, Default, Deserialize)]
struct TranscriptMessage {
    #[serde(default)]
    model: Option<String>,
    #[serde(default, deserialize_with = "Content::deserialize")]
    content: Option<Content>,
}

impl TranscriptRecord {
    fn is_hidden(&self) -> bool {
        self.is_sidechain == Some(true)
            || self.is_meta == Some(true)
            || self.is_compact_summary == Some(true)
    }

    /// Whether this record carries a visible message, used to tell a real
    /// conversation from a session that only wrote setup metadata.
    fn is_visible_message(&self) -> bool {
        if self.is_hidden() {
            return false;
        }
        if !matches!(self.kind.as_deref(), Some("user" | "assistant")) {
            return false;
        }
        self.message
            .as_ref()
            .and_then(|message| message.content.as_ref())
            .is_some_and(|content| !content.visible_text().is_empty())
    }
}

/// Parses one conversation, streaming its records one at a time.
pub(crate) fn parse(path: &Path) -> Result<ParsedConversation, SkipReason> {
    let reader = open_reader(path)?;
    let fallback = file_time(path);
    let mut records = RecordReader::new(reader);
    let mut count = 0usize;
    let mut cwd: Option<PathBuf> = None;
    let mut session_id = file_session(path);
    let mut model: Option<String> = None;
    let mut custom_title: Option<Option<String>> = None;
    let mut ai_title: Option<String> = None;
    let mut last_seen: Option<DateTime<Utc>> = None;
    let mut history = History::new();
    let mut has_model_reply = false;
    let mut has_real_prompt = false;

    loop {
        let record = match records.next::<TranscriptRecord>() {
            Ok(Record::Parsed(record)) => record,
            Ok(Record::End) => break,
            Ok(Record::Blank | Record::Malformed) => continue,
            Err(_) => return Err(SkipReason::Unreadable),
        };
        count += 1;
        super::record_limit(count)?;
        if let Some(value) = record
            .cwd
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
        {
            cwd = Some(PathBuf::from(value));
        }
        // The first id names the conversation. A transcript concatenated from
        // two sessions keeps the first, exactly as `peek` and the discovery
        // scan read it.
        if session_id.is_none()
            && let Some(value) = record
                .session_id
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty())
        {
            session_id = Some(value.to_owned());
        }
        if let Some(value) = record.ai_title.as_deref() {
            let value = value.trim();
            if !value.is_empty() {
                ai_title = Some(value.to_owned()).filter(|title| title_is_visible(title));
            }
        }
        if let Some(value) = record.custom_title.as_deref() {
            let value = value.trim();
            // Claude flushes the AI title after a rename; an empty rename clears
            // the custom title.
            custom_title = Some(if value.is_empty() || !title_is_visible(value) {
                None
            } else {
                Some(value.to_owned())
            });
        }
        if let Some(at) = parse_time(record.timestamp.as_deref()) {
            last_seen = Some(last_seen.map_or(at, |seen| seen.max(at)));
        }
        if record.is_hidden() {
            continue;
        }
        let kind = record.kind.as_deref().unwrap_or_default();
        if kind != "user" && kind != "assistant" {
            continue;
        }
        let Some(message) = record.message else {
            continue;
        };
        let Some(content) = message.content else {
            continue;
        };
        let text = content.visible_text();
        if text.is_empty() {
            continue;
        }
        let at = parse_time(record.timestamp.as_deref()).unwrap_or(fallback);
        let (role, text) = if kind == "user" {
            let Some(turn) = claude_user_text(&content) else {
                continue;
            };
            if turn.is_prompt {
                has_real_prompt = true;
            }
            (MessageRole::User, turn.text)
        } else {
            let recorded = message.model.as_deref().map(str::trim).unwrap_or_default();
            if !recorded.is_empty() && recorded != SYNTHETIC_MODEL {
                model = Some(recorded.to_owned());
                has_model_reply = true;
            }
            (MessageRole::Assistant, text)
        };
        history.push(KeptMessage { role, text, at });
    }

    let Some(cwd) = cwd else {
        return Err(SkipReason::NoWorkingDirectory);
    };
    // A transcript with no model reply is still a conversation when the person
    // asked something; only commands with no reply (a bare `/clear`) are not.
    if !has_model_reply && !has_real_prompt {
        return Err(SkipReason::OnlyCommands);
    }
    let first_message_at = history.first_at();
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

/// What one Claude user turn shows, and whether the person wrote a real prompt.
struct UserTurn {
    text: String,
    /// False for a slash command or a `!` shell command.
    is_prompt: bool,
}

/// Claude Code's own UI element names.
const WRAPPER_TAGS: &[&str] = &[
    "task-notification",
    "local-command-stdout",
    "local-command-stderr",
    "local-command-caveat",
    "bash-stdout",
    "bash-stderr",
    "bash-input",
    "command-name",
    "command-message",
    "command-args",
];

/// One complete `<tag>…</tag>` element of a block.
struct WrapperElement<'a> {
    tag: &'a str,
    body: &'a str,
}

/// Every element a block is made of, or `None` when it is not markup.
///
/// The whole block must be covered by complete, known elements: a prompt that
/// only looks like one of them is ordinary text and stays.
fn parse_wrappers(text: &str) -> Option<Vec<WrapperElement<'_>>> {
    let mut rest = text.trim();
    if rest.is_empty() {
        return None;
    }
    let mut elements = Vec::new();
    loop {
        rest = rest.trim_start();
        if rest.is_empty() {
            break;
        }
        let after_open = rest.strip_prefix('<')?;
        let name_end =
            after_open.find(|character: char| character.is_whitespace() || character == '>')?;
        let tag = &after_open[..name_end];
        if !WRAPPER_TAGS.contains(&tag) {
            return None;
        }
        let open_end = after_open.find('>')?;
        let body_and_rest = &after_open[open_end + 1..];
        let close = format!("</{tag}>");
        let body_end = body_and_rest.find(&close)?;
        elements.push(WrapperElement {
            tag,
            body: &body_and_rest[..body_end],
        });
        rest = &body_and_rest[body_end + close.len()..];
    }
    (!elements.is_empty()).then_some(elements)
}

/// What a user turn shows, or `None` for Claude Code's own UI output.
///
/// Slash commands and `!` shell commands are kept as the person typed them; the
/// command's output, background task notifications and interruption markers are
/// left out. Ordinary text keeps its exact bytes, so a prompt is never trimmed.
fn claude_user_text(content: &Content) -> Option<UserTurn> {
    let mut ordinary: Vec<String> = Vec::new();
    let mut name: Option<String> = None;
    let mut args: Option<String> = None;
    let mut shell: Option<String> = None;
    for RawText { kind, text } in content.raw_texts() {
        if !is_visible_block(&kind) {
            continue;
        }
        let trimmed = text.trim();
        if trimmed == "[Request interrupted by user]"
            || trimmed == "[Request interrupted by user for tool use]"
        {
            continue;
        }
        let Some(elements) = parse_wrappers(trimmed) else {
            ordinary.push(text);
            continue;
        };
        for element in elements {
            match element.tag {
                "command-name" => name = Some(element.body.trim().to_owned()),
                "command-message" if name.is_none() => {
                    name = Some(format!("/{}", element.body.trim()));
                }
                "command-args" => args = Some(element.body.to_owned()),
                "bash-input" => shell = Some(format!("!{}", element.body)),
                // Output and notifications are Claude Code's, not the person's.
                _ => {}
            }
        }
    }
    if let Some(name) = name.filter(|name| !name.is_empty()) {
        let text = match args {
            Some(args) if !args.trim().is_empty() => format!("{name} {args}"),
            _ => name,
        };
        return Some(UserTurn {
            text,
            is_prompt: false,
        });
    }
    if let Some(shell) = shell {
        return Some(UserTurn {
            text: shell,
            is_prompt: false,
        });
    }
    if ordinary.is_empty() {
        return None;
    }
    Some(UserTurn {
        text: ordinary.join("\n"),
        is_prompt: true,
    })
}

fn is_visible_block(kind: &str) -> bool {
    matches!(kind, "" | "text" | "input_text" | "output_text")
}

/// A saved title that is really a user turn's markup is not shown as a title.
fn title_is_visible(value: &str) -> bool {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return false;
    }
    if trimmed == "[Request interrupted by user]"
        || trimmed == "[Request interrupted by user for tool use]"
    {
        return false;
    }
    parse_wrappers(trimmed).is_none()
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

    fn write_transcript(lines: &[serde_json::Value]) -> (tempfile::TempDir, PathBuf) {
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

    fn record(kind: &str, text: &str, model: Option<&str>) -> serde_json::Value {
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
            record("user", "<bash-input> ls -la</bash-input>", None),
            record("assistant", "listed", Some("claude-sonnet-5")),
        ]);
        let parsed = parse(&path).expect("parse");
        let texts: Vec<&str> = parsed
            .messages
            .iter()
            .map(|message| message.text.as_str())
            .collect();
        assert_eq!(texts, ["/review src/", "reviewed", "! ls -la", "listed"]);
        assert_eq!(parsed.model.as_deref(), Some("claude-sonnet-5"));
        assert_eq!(parsed.session_id.as_deref(), Some("session-1"));
    }

    #[test]
    fn a_literal_prompt_that_starts_like_a_notification_stays() {
        let literal = "<task-notification is not what I meant; please explain it>";
        let (_dir, path) = write_transcript(&[
            record("user", literal, None),
            record("assistant", "explained", Some("claude-sonnet-5")),
        ]);
        let parsed = parse(&path).expect("parse");
        assert_eq!(parsed.messages[0].text, literal);
    }

    #[test]
    fn a_complete_notification_block_is_not_a_prompt() {
        let (_dir, path) = write_transcript(&[
            record(
                "user",
                "<task-notification>\n<task-id>abc</task-id>\n</task-notification>",
                None,
            ),
            record("user", "real request", None),
            record("assistant", "done", Some("claude-sonnet-5")),
        ]);
        let parsed = parse(&path).expect("parse");
        let texts: Vec<&str> = parsed
            .messages
            .iter()
            .map(|message| message.text.as_str())
            .collect();
        assert_eq!(texts, ["real request", "done"]);
    }

    #[test]
    fn a_prompt_without_a_reply_is_imported_and_a_clear_only_transcript_is_not() {
        let (_dir, path) = write_transcript(&[record("user", "start on the parser", None)]);
        let parsed = parse(&path).expect("prompt without a reply");
        assert_eq!(parsed.messages[0].text, "start on the parser");

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
    fn a_five_megabyte_prompt_is_kept_whole() {
        let prompt = "x".repeat(5 * 1024 * 1024);
        let (_dir, path) = write_transcript(&[
            record("user", &prompt, None),
            record("assistant", "ok", Some("claude-sonnet-5")),
        ]);
        let parsed = parse(&path).expect("parse");
        assert_eq!(parsed.messages[0].text.len(), prompt.len());
    }

    #[test]
    fn a_short_prompt_beside_a_large_attachment_is_kept() {
        let (_dir, path) = write_transcript(&[
            json!({
                "type": "user",
                "sessionId": "session-1",
                "cwd": "/home/user/code/api",
                "timestamp": "2026-11-20T10:00:00Z",
                "message": {
                    "role": "user",
                    "content": [
                        {"type": "text", "text": "what is in this screenshot?"},
                        {"type": "image", "source": {
                            "type": "base64",
                            "media_type": "image/png",
                            "data": "A".repeat(6 * 1024 * 1024),
                        }},
                    ],
                },
            }),
            record("assistant", "a chart", Some("claude-sonnet-5")),
        ]);
        let parsed = parse(&path).expect("parse");
        assert_eq!(parsed.messages[0].text, "what is in this screenshot?");
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
