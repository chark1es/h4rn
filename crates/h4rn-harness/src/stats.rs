//! What a tool call did, as one number: a diffstat for edits, a match count
//! for searches, a pass count for test runs.
//!
//! Every adapter normalises its vendor's tool events into
//! [`h4rn_protocol::ToolCall`]; this module is the one place that reads a
//! finished call's input and result and turns them into
//! [`h4rn_protocol::ToolStats`], so the UI never parses tool output. Each
//! adapter calls [`derive`] with whatever structure its protocol gives it:
//!
//! | adapter | edits | searches |
//! |---|---|---|
//! | Claude Code | `structuredPatch` in the result, else the `Edit`/`Write`/`MultiEdit` input | `Grep`/`Glob` result summary, else the output text |
//! | Codex | `fileChange` unified diffs | none (searches are shell commands) |
//! | OpenCode | `metadata.filediff` / `metadata.diff`, else the `edit`/`write` input | `Found N matches` text, `glob` file lists |
//! | Antigravity (ACP) | the `diff` content items (old and new text) | the search output text |
//!
//! Counting is best effort by design: a tool that reports neither a patch nor
//! a countable result gets `None`, never a guess.

use h4rn_protocol::{MatchUnit, ToolStats};
use serde_json::Value;

/// Lines compared by the line differ. Past this, the diff is a size
/// difference rather than a true LCS, which keeps a pasted megabyte from
/// stalling the event pump.
const MAX_DIFF_LINES: usize = 2_000;

/// What kind of tool a name denotes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    Edit,
    Search,
    Shell,
    Other,
}

fn kind_of(name: &str) -> Kind {
    let lower = name.to_ascii_lowercase();
    // MCP tools are somebody else's vocabulary: `mcp__server__edit` is not an
    // edit of ours.
    if lower.starts_with("mcp__") || lower.starts_with("mcp:") {
        return Kind::Other;
    }
    const EDITS: [&str; 12] = [
        "edit",
        "write",
        "multiedit",
        "notebookedit",
        "apply_patch",
        "applypatch",
        "patch",
        "create",
        "write_file",
        "edit_file",
        "str_replace",
        "str_replace_editor",
    ];
    const SEARCHES: [&str; 11] = [
        "grep",
        "glob",
        "search",
        "ripgrep",
        "grep_search",
        "codebase_search",
        "file_search",
        "find_by_name",
        "find_file",
        "list_files",
        "find",
    ];
    const SHELLS: [&str; 6] = [
        "bash",
        "shell",
        "exec_command",
        "run_command",
        "run_terminal_cmd",
        "terminal",
    ];
    if EDITS.contains(&lower.as_str()) {
        Kind::Edit
    } else if SEARCHES.contains(&lower.as_str()) {
        Kind::Search
    } else if SHELLS.contains(&lower.as_str()) {
        Kind::Shell
    } else {
        Kind::Other
    }
}

/// Derives the stats for a finished tool call.
///
/// `structured` is the protocol's own structured result when it has one:
/// Claude Code's `tool_use_result`, OpenCode's tool `metadata`.
pub fn derive(
    name: &str,
    input: Option<&Value>,
    output: Option<&str>,
    structured: Option<&Value>,
) -> Option<ToolStats> {
    match kind_of(name) {
        Kind::Edit => edit_stats(input, structured),
        Kind::Search => search_stats(output, structured),
        Kind::Shell => output.and_then(test_stats),
        Kind::Other => None,
    }
}

// ---------------------------------------------------------------------------
// Test runs
// ---------------------------------------------------------------------------

/// The pass and fail counts a test runner printed ("Tests  48 passed (48)",
/// "2 failed, 46 passed", "test result: ok. 48 passed; 0 failed"). A command
/// that printed none of these gets `None`, never a guess.
fn test_stats(output: &str) -> Option<ToolStats> {
    // Cargo prints one `test result:` line per test binary; add them up.
    let cargo: Vec<(u32, u32)> = output
        .lines()
        .filter(|line| line.trim_start().starts_with("test result:"))
        .filter_map(counts_in)
        .collect();
    if !cargo.is_empty() {
        let passed = cargo.iter().map(|(passed, _)| passed).sum();
        let failed = cargo.iter().map(|(_, failed)| failed).sum();
        return Some(ToolStats::Tests { passed, failed });
    }
    // Everything else prints a summary near the end; the last one wins.
    output
        .lines()
        .rev()
        .find_map(counts_in)
        .map(|(passed, failed)| ToolStats::Tests { passed, failed })
}

/// `(passed, failed)` from one summary line, or `None` when it has neither.
fn counts_in(line: &str) -> Option<(u32, u32)> {
    let words: Vec<&str> = line
        .split(|c: char| c.is_whitespace() || matches!(c, ',' | ';' | '(' | ')' | ':'))
        .filter(|word| !word.is_empty())
        .collect();
    let (mut passed, mut failed, mut seen) = (0u32, 0u32, false);
    for pair in words.windows(2) {
        let Ok(count) = pair[0].parse::<u32>() else {
            continue;
        };
        match pair[1] {
            "passed" => {
                passed = passed.saturating_add(count);
                seen = true;
            }
            "failed" => {
                failed = failed.saturating_add(count);
                seen = true;
            }
            _ => {}
        }
    }
    seen.then_some((passed, failed))
}

// ---------------------------------------------------------------------------
// Edits
// ---------------------------------------------------------------------------

fn edit_stats(input: Option<&Value>, structured: Option<&Value>) -> Option<ToolStats> {
    if let Some(stats) = structured.and_then(structured_edit) {
        return Some(stats);
    }
    let input = input?;
    // Codex `fileChange`: `changes: [{path, kind, diff}]`.
    if let Some(changes) = input.get("changes").and_then(Value::as_array) {
        let (mut added, mut removed) = (0u32, 0u32);
        let mut any = false;
        for change in changes {
            if let Some((a, r)) = change_stats(change) {
                added = added.saturating_add(a);
                removed = removed.saturating_add(r);
                any = true;
            }
        }
        return any.then_some(ToolStats::Diff { added, removed });
    }
    // `MultiEdit`: a list of replacements in one file.
    if let Some(edits) = input.get("edits").and_then(Value::as_array) {
        let (mut added, mut removed) = (0u32, 0u32);
        let mut any = false;
        for edit in edits {
            if let Some((old, new)) = replacement(edit) {
                let (a, r) = line_diff(old, new);
                added = added.saturating_add(a);
                removed = removed.saturating_add(r);
                any = true;
            }
        }
        return any.then_some(ToolStats::Diff { added, removed });
    }
    if let Some((old, new)) = replacement(input) {
        let (added, removed) = line_diff(old, new);
        return Some(ToolStats::Diff { added, removed });
    }
    // A whole-file write: everything is new. Only trusted when the result gave
    // no patch (a write over an existing file would otherwise under-count
    // removals, which is why structured results win above).
    let content = ["content", "file_text", "text", "contents"]
        .iter()
        .find_map(|key| input.get(*key).and_then(Value::as_str))?;
    Some(ToolStats::Diff {
        added: count_lines(content),
        removed: 0,
    })
}

/// A replacement pair in the spellings the harnesses use.
fn replacement(value: &Value) -> Option<(&str, &str)> {
    let pair = |old: &str, new: &str| -> Option<(&str, &str)> {
        Some((value.get(old)?.as_str()?, value.get(new)?.as_str()?))
    };
    pair("old_string", "new_string")
        .or_else(|| pair("oldString", "newString"))
        .or_else(|| pair("old_str", "new_str"))
        .or_else(|| pair("oldText", "newText"))
        .or_else(|| pair("old_text", "new_text"))
}

/// A Codex `fileChange` entry.
fn change_stats(change: &Value) -> Option<(u32, u32)> {
    let diff = change.get("diff")?.as_str()?;
    let kind = change
        .get("kind")
        .and_then(|kind| kind.as_str().or_else(|| kind.get("type")?.as_str()))
        .unwrap_or("update");
    if looks_unified(diff) {
        return Some(unified_stats(diff));
    }
    // Adds and deletes carry the whole file as the "diff".
    match kind {
        "add" | "create" => Some((count_lines(diff), 0)),
        "delete" | "remove" => Some((0, count_lines(diff))),
        _ => None,
    }
}

/// The protocol's own structured result for an edit.
fn structured_edit(structured: &Value) -> Option<ToolStats> {
    // OpenCode: `metadata.filediff.{additions,deletions}`.
    let filediff = structured
        .get("filediff")
        .or_else(|| structured.get("metadata").and_then(|m| m.get("filediff")));
    if let Some(filediff) = filediff
        && let (Some(added), Some(removed)) = (
            filediff.get("additions").and_then(Value::as_u64),
            filediff.get("deletions").and_then(Value::as_u64),
        )
    {
        return Some(ToolStats::Diff {
            added: clamp(added),
            removed: clamp(removed),
        });
    }
    // OpenCode: `metadata.diff` is a unified diff.
    if let Some(diff) = structured
        .get("diff")
        .or_else(|| structured.get("metadata").and_then(|m| m.get("diff")))
        .and_then(Value::as_str)
        && looks_unified(diff)
    {
        let (added, removed) = unified_stats(diff);
        return Some(ToolStats::Diff { added, removed });
    }
    // Claude Code: `structuredPatch: [{lines: ["-old", "+new", " ctx"]}]`.
    if let Some(hunks) = structured.get("structuredPatch").and_then(Value::as_array) {
        let (mut added, mut removed) = (0u32, 0u32);
        for hunk in hunks {
            for line in hunk
                .get("lines")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
            {
                match line.as_bytes().first() {
                    Some(b'+') => added = added.saturating_add(1),
                    Some(b'-') => removed = removed.saturating_add(1),
                    _ => {}
                }
            }
        }
        // An empty patch on a "create" result is the whole file.
        if hunks.is_empty() && structured.get("type").and_then(Value::as_str) == Some("create") {
            let content = structured.get("content").and_then(Value::as_str)?;
            return Some(ToolStats::Diff {
                added: count_lines(content),
                removed: 0,
            });
        }
        return Some(ToolStats::Diff { added, removed });
    }
    None
}

/// Counts `+`/`-` lines of a unified diff, ignoring the file headers.
pub fn unified_stats(diff: &str) -> (u32, u32) {
    let (mut added, mut removed) = (0u32, 0u32);
    for line in diff.lines() {
        if line.starts_with("+++") || line.starts_with("---") {
            continue;
        }
        match line.as_bytes().first() {
            Some(b'+') => added = added.saturating_add(1),
            Some(b'-') => removed = removed.saturating_add(1),
            _ => {}
        }
    }
    (added, removed)
}

fn looks_unified(text: &str) -> bool {
    text.lines()
        .any(|line| line.starts_with("@@") && line.len() > 2 || line.starts_with("+++ "))
}

fn count_lines(text: &str) -> u32 {
    if text.is_empty() {
        return 0;
    }
    clamp(text.lines().count() as u64)
}

fn clamp(value: u64) -> u32 {
    u32::try_from(value).unwrap_or(u32::MAX)
}

/// Lines added and removed turning `old` into `new`, by longest common
/// subsequence of lines.
pub fn line_diff(old: &str, new: &str) -> (u32, u32) {
    let old: Vec<&str> = old.lines().collect();
    let new: Vec<&str> = new.lines().collect();
    // Trim the common prefix and suffix: edits are usually a small change in
    // a few lines of context.
    let prefix = old.iter().zip(&new).take_while(|(a, b)| a == b).count();
    let (old, new) = (&old[prefix..], &new[prefix..]);
    let suffix = old
        .iter()
        .rev()
        .zip(new.iter().rev())
        .take_while(|(a, b)| a == b)
        .count();
    let (old, new) = (&old[..old.len() - suffix], &new[..new.len() - suffix]);
    if old.is_empty() || new.is_empty() {
        return (clamp(new.len() as u64), clamp(old.len() as u64));
    }
    if old.len() > MAX_DIFF_LINES || new.len() > MAX_DIFF_LINES {
        // Too big to diff exactly: report the net change, not a made-up LCS.
        return (
            clamp(new.len().saturating_sub(old.len()) as u64),
            clamp(old.len().saturating_sub(new.len()) as u64),
        );
    }
    let lcs = lcs_len(old, new);
    (
        clamp((new.len() - lcs) as u64),
        clamp((old.len() - lcs) as u64),
    )
}

fn lcs_len(a: &[&str], b: &[&str]) -> usize {
    let mut previous = vec![0usize; b.len() + 1];
    let mut current = vec![0usize; b.len() + 1];
    for left in a {
        for (j, right) in b.iter().enumerate() {
            current[j + 1] = if left == right {
                previous[j] + 1
            } else {
                previous[j + 1].max(current[j])
            };
        }
        std::mem::swap(&mut previous, &mut current);
    }
    previous[b.len()]
}

// ---------------------------------------------------------------------------
// Searches
// ---------------------------------------------------------------------------

fn search_stats(output: Option<&str>, structured: Option<&Value>) -> Option<ToolStats> {
    if let Some(stats) = structured.and_then(structured_search) {
        return Some(stats);
    }
    let output = output?.trim();
    if output.is_empty() {
        return Some(ToolStats::Matches {
            count: 0,
            unit: MatchUnit::Matches,
        });
    }
    // "Found 3 files", "Found 12 matches", "Found 7 results".
    if let Some(rest) = output.strip_prefix("Found ") {
        let mut words = rest.split_whitespace();
        if let (Some(number), Some(noun)) = (words.next(), words.next())
            && let Ok(count) = number.replace(',', "").parse::<u32>()
        {
            let unit = if noun.starts_with("file") || noun.starts_with("path") {
                MatchUnit::Files
            } else {
                MatchUnit::Matches
            };
            return Some(ToolStats::Matches { count, unit });
        }
    }
    if output.eq_ignore_ascii_case("no matches found")
        || output.eq_ignore_ascii_case("no files found")
        || output.eq_ignore_ascii_case("no results found")
    {
        return Some(ToolStats::Matches {
            count: 0,
            unit: MatchUnit::Matches,
        });
    }
    // A bare list: one result per line (a glob, or `files_with_matches`).
    let lines = output
        .lines()
        .filter(|line| !line.trim().is_empty())
        .filter(|line| !line.starts_with('[') && !line.starts_with("(Results"))
        .count();
    let paths_only = output
        .lines()
        .filter(|line| !line.trim().is_empty())
        .all(|line| !line.contains(':') || line.starts_with('/') || line.starts_with("./"));
    Some(ToolStats::Matches {
        count: clamp(lines as u64),
        unit: if paths_only {
            MatchUnit::Files
        } else {
            MatchUnit::Matches
        },
    })
}

/// Claude Code's structured `Grep`/`Glob` result.
fn structured_search(structured: &Value) -> Option<ToolStats> {
    let number = |key: &str| structured.get(key).and_then(Value::as_u64).map(clamp);
    match structured.get("mode").and_then(Value::as_str) {
        Some("content") => number("numLines").map(|count| ToolStats::Matches {
            count,
            unit: MatchUnit::Matches,
        }),
        Some("count") => number("numMatches").map(|count| ToolStats::Matches {
            count,
            unit: MatchUnit::Matches,
        }),
        Some("files_with_matches") => number("numFiles").map(|count| ToolStats::Matches {
            count,
            unit: MatchUnit::Files,
        }),
        _ => number("numFiles").map(|count| ToolStats::Matches {
            count,
            unit: MatchUnit::Files,
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn diff(added: u32, removed: u32) -> Option<ToolStats> {
        Some(ToolStats::Diff { added, removed })
    }

    fn found(count: u32, unit: MatchUnit) -> Option<ToolStats> {
        Some(ToolStats::Matches { count, unit })
    }

    #[test]
    fn test_runs_report_pass_and_fail_counts() {
        let tests = |passed, failed| Some(ToolStats::Tests { passed, failed });
        assert_eq!(
            test_stats(" Test Files  3 passed (3)\n      Tests  48 passed (48)\n"),
            tests(48, 0)
        );
        assert_eq!(
            test_stats("Tests: 2 failed, 46 passed, 48 total"),
            tests(46, 2)
        );
        assert_eq!(
            test_stats(
                "test result: ok. 10 passed; 0 failed; 0 ignored\ntest result: FAILED. 5 passed; 1 failed;"
            ),
            tests(15, 1)
        );
        assert_eq!(test_stats("compiled in 2s\nno summary here"), None);
        assert_eq!(
            derive("Bash", None, Some("Tests  48 passed (48)"), None),
            tests(48, 0)
        );
        assert_eq!(derive("Bash", None, Some("ls -la"), None), None);
    }

    #[test]
    fn line_diff_counts_a_replacement_not_the_blocks() {
        assert_eq!(line_diff("a\nb\nc", "a\nB\nc"), (1, 1));
        assert_eq!(line_diff("a\nb", "a\nb\nc\nd"), (2, 0));
        assert_eq!(line_diff("a\nb\nc", "a"), (0, 2));
        assert_eq!(line_diff("", "x\ny"), (2, 0));
        assert_eq!(line_diff("same", "same"), (0, 0));
        // Moving a line is one removal and one addition.
        assert_eq!(line_diff("a\nb\nc", "b\nc\na"), (1, 1));
    }

    #[test]
    fn huge_inputs_fall_back_to_the_net_change() {
        let old = "x\n".repeat(MAX_DIFF_LINES + 10);
        let new = "y\n".repeat(MAX_DIFF_LINES + 20);
        assert_eq!(line_diff(&old, &new), (10, 0));
    }

    #[test]
    fn unified_stats_ignore_headers_and_context() {
        let patch = "--- a/x\n+++ b/x\n@@ -1,3 +1,3 @@\n ctx\n-old\n+new\n+extra\n";
        assert_eq!(unified_stats(patch), (2, 1));
    }

    #[test]
    fn claude_edit_uses_the_structured_patch_then_the_input() {
        let structured = json!({
            "type": "update",
            "structuredPatch": [{"lines": [" a", "-b", "+B", "+C", " d"]}]
        });
        assert_eq!(
            derive("Edit", Some(&json!({})), None, Some(&structured)),
            diff(2, 1)
        );
        let input = json!({"file_path": "/w/a", "old_string": "b\n", "new_string": "B\nC\n"});
        assert_eq!(derive("Edit", Some(&input), None, None), diff(2, 1));
    }

    #[test]
    fn claude_write_counts_the_file_unless_the_result_says_otherwise() {
        let input = json!({"file_path": "/w/a", "content": "one\ntwo\nthree\n"});
        assert_eq!(derive("Write", Some(&input), None, None), diff(3, 0));
        // Writing over an existing file: the result's patch knows the removals.
        let structured = json!({
            "type": "update",
            "structuredPatch": [{"lines": ["-old1", "-old2", "+one", "+two", "+three"]}]
        });
        assert_eq!(
            derive("Write", Some(&input), None, Some(&structured)),
            diff(3, 2)
        );
        let created = json!({"type": "create", "structuredPatch": [], "content": "a\nb\n"});
        assert_eq!(derive("Write", None, None, Some(&created)), diff(2, 0));
    }

    #[test]
    fn multiedit_sums_its_replacements() {
        let input = json!({"edits": [
            {"old_string": "a", "new_string": "b"},
            {"old_string": "c\nd", "new_string": "c"}
        ]});
        assert_eq!(derive("MultiEdit", Some(&input), None, None), diff(1, 2));
    }

    #[test]
    fn codex_file_changes_read_the_unified_diffs() {
        let input = json!({"changes": [
            {"path": "/w/a.rs", "kind": "update", "diff": "@@ -1 +1,2 @@\n-old\n+new\n+more\n"},
            {"path": "/w/b.rs", "kind": "add", "diff": "x\ny\n"},
            {"path": "/w/c.rs", "kind": {"type": "delete"}, "diff": "gone\n"}
        ]});
        assert_eq!(derive("apply_patch", Some(&input), None, None), diff(4, 2));
        // The trimmed fixture diff (`@@` alone) carries no counts.
        let trimmed = json!({"changes": [{"path": "/w/a", "kind": "update", "diff": "@@"}]});
        assert_eq!(derive("apply_patch", Some(&trimmed), None, None), None);
    }

    #[test]
    fn opencode_prefers_filediff_then_the_diff_then_the_input() {
        let metadata = json!({"filediff": {"additions": 4, "deletions": 1}});
        assert_eq!(derive("edit", None, None, Some(&metadata)), diff(4, 1));
        let metadata = json!({"diff": "--- a\n+++ b\n@@ -1 +1 @@\n-x\n+y\n"});
        assert_eq!(derive("edit", None, None, Some(&metadata)), diff(1, 1));
        let input = json!({"filePath": "/w/a", "oldString": "x", "newString": "y\nz"});
        assert_eq!(derive("edit", Some(&input), None, None), diff(2, 1));
    }

    #[test]
    fn searches_count_what_the_tool_reported() {
        assert_eq!(
            derive("Grep", None, Some("Found 3 files\n/w/a\n/w/b\n/w/c"), None),
            found(3, MatchUnit::Files)
        );
        assert_eq!(
            derive(
                "grep",
                None,
                Some("Found 12 matches\n/w/a:\n  Line 1: x"),
                None
            ),
            found(12, MatchUnit::Matches)
        );
        assert_eq!(
            derive("Grep", None, Some("No matches found"), None),
            found(0, MatchUnit::Matches)
        );
        assert_eq!(
            derive("Glob", None, Some("/w/a.rs\n/w/b.rs"), None),
            found(2, MatchUnit::Files)
        );
        assert_eq!(
            derive("Grep", None, Some("a.rs:10:fn x\na.rs:20:fn y"), None),
            found(2, MatchUnit::Matches)
        );
    }

    #[test]
    fn claude_structured_search_results_win() {
        let content = json!({"mode": "content", "numLines": 14, "numFiles": 2});
        assert_eq!(
            derive("Grep", None, Some("..."), Some(&content)),
            found(14, MatchUnit::Matches)
        );
        let count = json!({"mode": "count", "numMatches": 9, "numFiles": 3});
        assert_eq!(
            derive("Grep", None, None, Some(&count)),
            found(9, MatchUnit::Matches)
        );
        let files = json!({"mode": "files_with_matches", "numFiles": 5});
        assert_eq!(
            derive("Grep", None, None, Some(&files)),
            found(5, MatchUnit::Files)
        );
        let glob = json!({"filenames": ["a", "b"], "numFiles": 2});
        assert_eq!(
            derive("Glob", None, None, Some(&glob)),
            found(2, MatchUnit::Files)
        );
    }

    #[test]
    fn other_tools_and_mcp_lookalikes_get_nothing() {
        assert_eq!(
            derive("Bash", Some(&json!({"command": "ls"})), Some("a"), None),
            None
        );
        assert_eq!(derive("Read", None, Some("1\ta"), None), None);
        assert_eq!(
            derive("mcp__github__search", None, Some("Found 3 files"), None),
            None
        );
        assert_eq!(
            derive("web_search", None, Some("Found 3 files"), None),
            None
        );
    }
}
