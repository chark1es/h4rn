//! The diffstat and match count each adapter derives from its own tool events.
//!
//! The frames are the shapes the harnesses emit (see the fixtures beside this
//! file); the structured results (`tool_use_result`, OpenCode `metadata`,
//! ACP `diff` content) are what carries the exact numbers.

use h4rn_harness::acp::AcpToolCall;
use h4rn_harness::{ClaudeStreamParser, CodexParser, HarnessEvent, OpenCodeParser};
use h4rn_protocol::{MatchUnit, ToolCall, ToolCallStatus, ToolStats};
use serde_json::{Value, json};

fn tool_calls(events: Vec<HarnessEvent>) -> Vec<ToolCall> {
    events
        .into_iter()
        .filter_map(|event| match event {
            HarnessEvent::ToolCall { tool_call } => Some(tool_call),
            _ => None,
        })
        .collect()
}

fn line(value: Value) -> String {
    value.to_string()
}

// ---------------------------------------------------------------------------
// Claude Code
// ---------------------------------------------------------------------------

fn claude_turn(name: &str, input: Value, result_text: &str, tool_use_result: Value) -> ToolCall {
    let mut parser = ClaudeStreamParser::new();
    let mut calls = Vec::new();
    calls.extend(tool_calls(parser.push_line(&line(json!({
        "type": "assistant",
        "message": {"model": "claude-opus-5-5", "content": [
            {"type": "tool_use", "id": "toolu_1", "name": name, "input": input}
        ]},
        "parent_tool_use_id": null, "session_id": "s"
    })))));
    calls.extend(tool_calls(parser.push_line(&line(json!({
        "type": "user",
        "message": {"role": "user", "content": [
            {"tool_use_id": "toolu_1", "type": "tool_result", "content": result_text}
        ]},
        "tool_use_result": tool_use_result,
        "parent_tool_use_id": null, "session_id": "s"
    })))));
    assert_eq!(calls.len(), 2, "a running event and a finished one");
    assert_eq!(calls[0].stats, None, "nothing is counted while it runs");
    calls.remove(1)
}

#[test]
fn claude_edit_uses_the_structured_patch() {
    let call = claude_turn(
        "Edit",
        json!({"file_path": "/w/a.rs", "old_string": "x", "new_string": "y"}),
        "The file /w/a.rs has been updated.",
        json!({"type": "update", "structuredPatch": [
            {"oldStart": 1, "oldLines": 3, "newStart": 1, "newLines": 4,
             "lines": [" a", "-b", "+B", "+C", " d"]}
        ]}),
    );
    assert_eq!(call.status, ToolCallStatus::Completed);
    assert_eq!(
        call.stats,
        Some(ToolStats::Diff {
            added: 2,
            removed: 1
        })
    );
}

#[test]
fn claude_edit_without_a_structured_result_counts_the_input() {
    let call = claude_turn(
        "Edit",
        json!({"file_path": "/w/a.rs", "old_string": "a\nb\n", "new_string": "a\nB\nC\n"}),
        "updated",
        Value::Null,
    );
    assert_eq!(
        call.stats,
        Some(ToolStats::Diff {
            added: 2,
            removed: 1
        })
    );
}

#[test]
fn claude_write_counts_the_new_file() {
    let call = claude_turn(
        "Write",
        json!({"file_path": "/w/new.txt", "content": "one\ntwo\nthree\n"}),
        "File created successfully",
        json!({"type": "create", "filePath": "/w/new.txt", "content": "one\ntwo\nthree\n", "structuredPatch": []}),
    );
    assert_eq!(
        call.stats,
        Some(ToolStats::Diff {
            added: 3,
            removed: 0
        })
    );
}

#[test]
fn claude_grep_reports_matches_or_files() {
    let content = claude_turn(
        "Grep",
        json!({"pattern": "TODO", "output_mode": "content"}),
        "a.rs:3:// TODO one\na.rs:9:// TODO two",
        json!({"mode": "content", "numFiles": 1, "numLines": 2, "content": "..."}),
    );
    assert_eq!(
        content.stats,
        Some(ToolStats::Matches {
            count: 2,
            unit: MatchUnit::Matches
        })
    );
    let files = claude_turn(
        "Grep",
        json!({"pattern": "TODO"}),
        "Found 3 files\n/w/a.rs\n/w/b.rs\n/w/c.rs",
        Value::Null,
    );
    assert_eq!(
        files.stats,
        Some(ToolStats::Matches {
            count: 3,
            unit: MatchUnit::Files
        })
    );
}

#[test]
fn claude_counts_a_long_result_not_its_truncated_preview() {
    // The preview is cut at a few KB; the count must come from the whole text.
    let paths: Vec<String> = (0..2_000)
        .map(|n| format!("/workspace/src/file_{n}.rs"))
        .collect();
    let call = claude_turn(
        "Glob",
        json!({"pattern": "**/*.rs"}),
        &paths.join("\n"),
        Value::Null,
    );
    assert_eq!(
        call.stats,
        Some(ToolStats::Matches {
            count: 2_000,
            unit: MatchUnit::Files
        })
    );
    assert!(
        call.output
            .as_deref()
            .is_some_and(|output| output.len() < 20_000)
    );
}

#[test]
fn claude_bash_and_failed_edits_have_no_stats() {
    let bash = claude_turn("Bash", json!({"command": "ls"}), "a\nb", Value::Null);
    assert_eq!(bash.stats, None);
}

// ---------------------------------------------------------------------------
// Codex
// ---------------------------------------------------------------------------

#[test]
fn codex_file_changes_count_their_unified_diffs() {
    let mut parser = CodexParser::new();
    let changes = json!([
        {"path": "/w/a.rs", "kind": "update",
         "diff": "--- a/w/a.rs\n+++ b/w/a.rs\n@@ -1,2 +1,3 @@\n keep\n-old\n+new\n+extra\n"},
        {"path": "/w/b.rs", "kind": "add", "diff": "x\ny\n"}
    ]);
    let started = tool_calls(parser.push_line(&line(json!({
        "method": "item/started",
        "params": {"item": {"type": "fileChange", "id": "p1", "status": "inProgress", "changes": changes},
                   "threadId": "t", "turnId": "u"}
    }))));
    assert_eq!(started[0].stats, None);
    let done = tool_calls(parser.push_line(&line(json!({
        "method": "item/completed",
        "params": {"item": {"type": "fileChange", "id": "p1", "status": "completed", "changes": changes},
                   "threadId": "t", "turnId": "u"}
    }))));
    assert_eq!(
        done[0].stats,
        Some(ToolStats::Diff {
            added: 4,
            removed: 1
        })
    );
}

#[test]
fn codex_shell_commands_are_not_searches() {
    let mut parser = CodexParser::new();
    let done = tool_calls(parser.push_line(&line(json!({
        "method": "item/completed",
        "params": {"item": {"type": "commandExecution", "id": "c1", "command": "rg TODO",
                            "status": "completed", "exitCode": 0, "aggregatedOutput": "a.rs:1:TODO\n"},
                   "threadId": "t", "turnId": "u"}
    }))));
    assert_eq!(done[0].stats, None);
}

// ---------------------------------------------------------------------------
// OpenCode
// ---------------------------------------------------------------------------

fn opencode_events(events: Vec<Value>) -> Vec<ToolCall> {
    let mut parser = OpenCodeParser::new();
    let mut out = Vec::new();
    for event in events {
        out.extend(tool_calls(parser.push_line(&format!("data: {event}"))));
    }
    out
}

fn opencode_tool(name: &str, input: Value, content: &str, metadata: Value) -> ToolCall {
    let calls = opencode_events(vec![
        json!({"type": "session.tool.input.started",
               "data": {"sessionID": "s", "assistantMessageID": "m", "id": "call_1", "name": name}}),
        json!({"type": "session.tool.called",
               "data": {"sessionID": "s", "assistantMessageID": "m", "id": "call_1", "input": input}}),
        json!({"type": "session.tool.success",
               "data": {"sessionID": "s", "assistantMessageID": "m", "id": "call_1",
                        "content": [{"type": "text", "text": content}], "metadata": metadata}}),
    ]);
    calls.into_iter().last().expect("a finished tool call")
}

#[test]
fn opencode_edit_reads_filediff_then_the_diff() {
    let call = opencode_tool(
        "edit",
        json!({"filePath": "/w/a.rs", "oldString": "x", "newString": "y"}),
        "Edit applied successfully.",
        json!({"filediff": {"file": "/w/a.rs", "additions": 5, "deletions": 2}}),
    );
    assert_eq!(call.status, ToolCallStatus::Completed);
    assert_eq!(
        call.stats,
        Some(ToolStats::Diff {
            added: 5,
            removed: 2
        })
    );

    let from_diff = opencode_tool(
        "edit",
        json!({"filePath": "/w/a.rs"}),
        "ok",
        json!({"diff": "--- a\n+++ b\n@@ -1 +1,2 @@\n-x\n+y\n+z\n"}),
    );
    assert_eq!(
        from_diff.stats,
        Some(ToolStats::Diff {
            added: 2,
            removed: 1
        })
    );
}

#[test]
fn opencode_grep_and_glob_count_results() {
    let grep = opencode_tool(
        "grep",
        json!({"pattern": "TODO"}),
        "Found 4 matches\n/w/a.rs:\n  Line 3: // TODO",
        Value::Null,
    );
    assert_eq!(
        grep.stats,
        Some(ToolStats::Matches {
            count: 4,
            unit: MatchUnit::Matches
        })
    );
    let glob = opencode_tool(
        "glob",
        json!({"pattern": "*.rs"}),
        "/w/a.rs\n/w/b.rs",
        Value::Null,
    );
    assert_eq!(
        glob.stats,
        Some(ToolStats::Matches {
            count: 2,
            unit: MatchUnit::Files
        })
    );
}

#[test]
fn opencode_shell_has_no_stats() {
    let shell = opencode_tool("shell", json!({"command": "echo hi"}), "hi\n", Value::Null);
    assert_eq!(shell.stats, None);
}

// ---------------------------------------------------------------------------
// Antigravity (ACP)
// ---------------------------------------------------------------------------

#[test]
fn antigravity_diff_content_gives_an_exact_diffstat() {
    let edit = AcpToolCall::from_value(&json!({
        "toolCallId": "e1", "kind": "edit", "status": "completed", "title": "Edit src/lib.rs",
        "content": [{"type": "diff", "path": "/w/src/lib.rs",
                     "oldText": "fn a() {}\nfn b() {}\n",
                     "newText": "fn a() {}\nfn b2() {}\nfn c() {}\n"}]
    }));
    assert_eq!(
        edit.stats(),
        Some(ToolStats::Diff {
            added: 2,
            removed: 1
        })
    );

    // No old text: a new file.
    let created = AcpToolCall::from_value(&json!({
        "toolCallId": "e2", "kind": "edit", "status": "in_progress",
        "content": [{"type": "diff", "path": "/w/new.rs", "newText": "a\nb\nc\n"}]
    }));
    assert_eq!(
        created.stats(),
        Some(ToolStats::Diff {
            added: 3,
            removed: 0
        })
    );
}

#[test]
fn antigravity_searches_count_once_finished() {
    let search = |status: &str| {
        AcpToolCall::from_value(&json!({
            "toolCallId": "s1", "kind": "search", "status": status,
            "content": [{"type": "content", "content": {"type": "text", "text": "Found 6 matches\na.rs:1: x"}}]
        }))
    };
    assert_eq!(search("in_progress").stats(), None);
    assert_eq!(
        search("completed").stats(),
        Some(ToolStats::Matches {
            count: 6,
            unit: MatchUnit::Matches
        })
    );
}
