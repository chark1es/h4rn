//! A minimal ACP agent used by daemon integration tests.
//!
//! It speaks just enough of the protocol for the Antigravity adapter: the
//! handshake, a session, one echo turn per prompt. A prompt containing
//! `H4RN_FIXTURE_EXIT` makes the process vanish mid-turn, which is how the
//! tests drive a dead transport without a real `agy` install. That death waits
//! for a gate file in the session's workspace, so a test can queue a follow-up
//! behind the turn before the transport dies.
//!
//! Other marker prompts model the deaths that happen outside a turn: an answer
//! followed by an exit (`H4RN_FIXTURE_DIE_BETWEEN_TURNS`) and an answer followed
//! by a stopped stdin reader and a rate-limit reset on stderr
//! (`H4RN_FIXTURE_STALL_STDIN`).
//!
//! Two more orderings cover a rate limit that only the exit report carries
//! after the turn already ended: an answer, a closed stdout and only then the
//! exit (`H4RN_FIXTURE_LIMIT_EXIT_AFTER_TURN`), and an idle close followed by
//! the exit (`H4RN_FIXTURE_LIMIT_EXIT_BETWEEN_TURNS`). The first answers with
//! an ordinary newline-terminated frame, exactly as a real peer does, and
//! `H4RN_FIXTURE_LIMIT_EXIT_AFTER_QUEUE` keeps the transport healthy until the
//! prompt queued behind the turn reaches it, acts on that prompt without
//! reporting a tool call, and only then exits, so a test can force the ordering
//! where the queue was promoted and read before the exit-only limit landed. A
//! mid-turn exit with no stderr at all (`H4RN_FIXTURE_SILENT_EXIT`) proves the
//! exit status still names the row when there is no stderr to outrank it. An
//! idle death with no EOF at all (`H4RN_FIXTURE_IDLE_EXIT_ONLY`) holds stdout
//! open from a keeper process, so only the exit report can announce it.

use std::io::{BufRead, Write};
use std::process::{Command, Stdio};

use serde_json::{Value, json};

fn main() {
    // A file in the working directory fails the process before the handshake,
    // so a test can watch one session start fail and then retry the very same
    // configuration without editing it again.
    if std::path::Path::new(".agy-fixture-fail-initialize").exists() {
        eprintln!("agy-fixture: failing before initialize");
        std::process::exit(3);
    }
    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout();
    let mut sessions = 0u64;
    // A turn the fixture is holding open for a permission answer or a cancel.
    let mut pending_prompt: Option<Value> = None;
    let mut pending_permission = false;

    // `H4RN_FIXTURE_INITIALIZE_GATE` holds the handshake until the named file
    // appears, so a test can edit the configured command while a probe is in
    // flight. The `<gate>.waiting` marker says the fixture is parked there.
    if let Ok(gate) = std::env::var("H4RN_FIXTURE_INITIALIZE_GATE") {
        let gate = std::path::PathBuf::from(gate);
        let _ = std::fs::write(gate.with_extension("waiting"), b"");
        for _ in 0..600 {
            if gate.exists() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
    }

    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(frame) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        let id = frame.get("id").cloned();
        let Some(method) = frame.get("method").and_then(Value::as_str) else {
            // The answer to our own `session/request_permission` request.
            if pending_permission {
                pending_permission = false;
                if let Some(prompt_id) = pending_prompt.take() {
                    let session_id = frame
                        .get("params")
                        .and_then(|params| params.get("sessionId"))
                        .and_then(Value::as_str)
                        .unwrap_or("fixture-session");
                    write_frame(
                        &mut stdout,
                        &json!({
                            "jsonrpc": "2.0",
                            "method": "session/update",
                            "params": {
                                "sessionId": session_id,
                                "update": {
                                    "sessionUpdate": "agent_message_chunk",
                                    "content": {"type": "text", "text": "permission answered"}
                                }
                            }
                        }),
                    );
                    reply(
                        &mut stdout,
                        Some(prompt_id),
                        json!({"stopReason": "end_turn"}),
                    );
                }
            }
            continue;
        };
        let params = frame.get("params").cloned().unwrap_or(Value::Null);

        match method {
            "initialize" => reply(
                &mut stdout,
                id,
                if std::env::var_os("H4RN_FIXTURE_RESUME_ONLY").is_some() {
                    // A resume-only agent: it can continue a conversation but
                    // has no `session/load`.
                    json!({
                        "protocolVersion": 1,
                        "agentInfo": {"name": "agy fixture", "version": env!("CARGO_PKG_VERSION")},
                        "agentCapabilities": {
                            "promptCapabilities": {"image": false, "embeddedContext": false},
                            "mcpCapabilities": {},
                            "sessionCapabilities": {"resume": {}},
                        }
                    })
                } else {
                    json!({
                        "protocolVersion": 1,
                        "agentInfo": {"name": "agy fixture", "version": env!("CARGO_PKG_VERSION")},
                        "agentCapabilities": {
                            "loadSession": true,
                            "promptCapabilities": {"image": false, "embeddedContext": false},
                            "mcpCapabilities": {},
                            "sessionCapabilities": {"load": {}, "close": {}}
                        }
                    })
                },
            ),
            "session/new" => {
                sessions += 1;
                if let Some(cwd) = params.get("cwd").and_then(Value::as_str) {
                    log_start(cwd, "new");
                }
                reply(
                    &mut stdout,
                    id,
                    session_reply(&format!("fixture-session-{sessions}")),
                );
            }
            "session/load" => {
                let session_id = params
                    .get("sessionId")
                    .and_then(Value::as_str)
                    .unwrap_or("fixture-session")
                    .to_string();
                if let Some(cwd) = params.get("cwd").and_then(Value::as_str) {
                    log_start(cwd, &format!("load {session_id}"));
                }
                // ACP replays the old conversation as `session/update` frames
                // before the load reply. `H4RN_FIXTURE_LOAD_REPLAY` makes the
                // replay longer than the client's bounded event queue, so a
                // reader that cannot drain it deadlocks before the reply.
                if let Ok(count) = std::env::var("H4RN_FIXTURE_LOAD_REPLAY")
                    && let Ok(count) = count.parse::<u64>()
                {
                    for index in 0..count {
                        write_frame(
                            &mut stdout,
                            &json!({
                                "jsonrpc": "2.0",
                                "method": "session/update",
                                "params": {
                                    "sessionId": session_id,
                                    "update": {
                                        "sessionUpdate": "agent_message_chunk",
                                        "content": {
                                            "type": "text",
                                            "text": format!("replayed history {index}")
                                        }
                                    }
                                }
                            }),
                        );
                    }
                }
                let result = if std::env::var_os("H4RN_FIXTURE_EMPTY_RESUME").is_some() {
                    json!({ "sessionId": session_id })
                } else {
                    session_reply(&session_id)
                };
                reply(&mut stdout, id, result);
            }
            "session/resume" => {
                let session_id = params
                    .get("sessionId")
                    .and_then(Value::as_str)
                    .unwrap_or("fixture-session")
                    .to_string();
                if let Some(cwd) = params.get("cwd").and_then(Value::as_str) {
                    log_start(cwd, &format!("resume {session_id}"));
                }
                // Some agents answer a resume without repeating the catalogue:
                // the thread's saved selection still applies.
                let result = if std::env::var_os("H4RN_FIXTURE_EMPTY_RESUME").is_some() {
                    json!({ "sessionId": session_id })
                } else {
                    session_reply(&session_id)
                };
                reply(&mut stdout, id, result);
            }
            "session/set_config_option"
            | "session/set_model"
            | "session/set_mode"
            | "session/close" => {
                reply(&mut stdout, id, Value::Null);
            }
            "session/prompt" => {
                let text = params
                    .get("prompt")
                    .and_then(Value::as_array)
                    .map(|items| {
                        items
                            .iter()
                            .filter_map(|item| item.get("text").and_then(Value::as_str))
                            .collect::<Vec<_>>()
                            .join("\n")
                    })
                    .unwrap_or_default();
                if text.contains("H4RN_FIXTURE_PERMISSION") {
                    let session_id = params
                        .get("sessionId")
                        .and_then(Value::as_str)
                        .unwrap_or("fixture-session");
                    write_frame(
                        &mut stdout,
                        &json!({
                            "jsonrpc": "2.0",
                            "method": "session/update",
                            "params": {
                                "sessionId": session_id,
                                "update": {
                                    "sessionUpdate": "tool_call",
                                    "toolCallId": "perm-call-1",
                                    "title": "Run the fixture command",
                                    "kind": "execute",
                                    "status": "pending",
                                    "rawInput": {"command": "printf fixture"}
                                }
                            }
                        }),
                    );
                    write_frame(
                        &mut stdout,
                        &json!({
                            "jsonrpc": "2.0",
                            "id": "perm-1",
                            "method": "session/request_permission",
                            "params": {
                                "sessionId": session_id,
                                "toolCall": {
                                    "toolCallId": "perm-call-1",
                                    "title": "Run the fixture command",
                                    "kind": "execute",
                                    "status": "pending",
                                    "rawInput": {"command": "printf fixture"}
                                },
                                "options": [
                                    {"optionId": "allow", "name": "Allow", "kind": "allow_once"},
                                    {"optionId": "deny", "name": "Deny", "kind": "reject_once"}
                                ]
                            }
                        }),
                    );
                    pending_prompt = id;
                    pending_permission = true;
                    continue;
                }
                if text.contains("H4RN_FIXTURE_TOOL") {
                    let session_id = params
                        .get("sessionId")
                        .and_then(Value::as_str)
                        .unwrap_or("fixture-session");
                    for (status, title) in [
                        ("in_progress", "Run the fixture command"),
                        ("completed", "Run the fixture command"),
                    ] {
                        let update = if status == "in_progress" {
                            json!({
                                "sessionId": session_id,
                                "update": {
                                    "sessionUpdate": "tool_call",
                                    "toolCallId": "tool-1",
                                    "title": title,
                                    "kind": "execute",
                                    "status": status,
                                    "rawInput": {"command": "printf fixture"}
                                }
                            })
                        } else {
                            json!({
                                "sessionId": session_id,
                                "update": {
                                    "sessionUpdate": "tool_call_update",
                                    "toolCallId": "tool-1",
                                    "status": status,
                                    "rawOutput": "fixture output"
                                }
                            })
                        };
                        write_frame(
                            &mut stdout,
                            &json!({
                                "jsonrpc": "2.0",
                                "method": "session/update",
                                "params": update
                            }),
                        );
                    }
                    write_frame(
                        &mut stdout,
                        &json!({
                            "jsonrpc": "2.0",
                            "method": "session/update",
                            "params": {
                                "sessionId": session_id,
                                "update": {
                                    "sessionUpdate": "agent_message_chunk",
                                    "content": {"type": "text", "text": format!("echo: {text}")}
                                }
                            }
                        }),
                    );
                    reply(
                        &mut stdout,
                        id,
                        json!({"stopReason": "end_turn", "usage": {"inputTokens": 3, "outputTokens": 2}}),
                    );
                    continue;
                }
                if text.contains("H4RN_FIXTURE_PLAN") {
                    let session_id = params
                        .get("sessionId")
                        .and_then(Value::as_str)
                        .unwrap_or("fixture-session");
                    write_frame(
                        &mut stdout,
                        &json!({
                            "jsonrpc": "2.0",
                            "method": "session/update",
                            "params": {
                                "sessionId": session_id,
                                "update": {
                                    "sessionUpdate": "plan",
                                    "entries": [
                                        {
                                            "content": "Inspect the workspace",
                                            "priority": "high",
                                            "status": "completed"
                                        },
                                        {
                                            "content": "Change the fixture",
                                            "priority": "medium",
                                            "status": "in_progress"
                                        }
                                    ]
                                }
                            }
                        }),
                    );
                    write_frame(
                        &mut stdout,
                        &json!({
                            "jsonrpc": "2.0",
                            "method": "session/update",
                            "params": {
                                "sessionId": session_id,
                                "update": {
                                    "sessionUpdate": "agent_message_chunk",
                                    "content": {"type": "text", "text": "plan: done"}
                                }
                            }
                        }),
                    );
                    reply(&mut stdout, id, json!({"stopReason": "end_turn"}));
                    continue;
                }
                if text.contains("H4RN_FIXTURE_CANCEL") {
                    // Hold the turn open until `session/cancel` arrives.
                    pending_prompt = id;
                    continue;
                }
                if text.contains("H4RN_FIXTURE_EXIT") {
                    // Vanish mid-turn: stdout closes, the daemon's client sees a
                    // dead transport and never gets a prompt result. The death
                    // waits for a gate file the test creates, so the test can
                    // first queue a follow-up behind this turn. The stderr line
                    // lets the test prove the failure row keeps the agent's own
                    // diagnostic text.
                    wait_for_exit_gate();
                    eprintln!("agy-fixture: the session died mid-turn");
                    std::process::exit(7);
                }
                if text.contains("H4RN_FIXTURE_CLOSE_STDOUT") {
                    // Close stdout first, then leave the only diagnostic on
                    // stderr and exit. The daemon wakes on the EOF and waits a
                    // bounded window for this report, which carries the line.
                    close_stdout();
                    eprintln!("agy-fixture: the session died after stdout closed");
                    std::process::exit(7);
                }
                if text.contains("H4RN_FIXTURE_SILENT_EXIT") {
                    // Close stdout and exit with no stderr at all: the row can
                    // only name the exit status, which must beat the generic
                    // stdout-close notice.
                    close_stdout();
                    std::process::exit(7);
                }
                if text.contains("H4RN_FIXTURE_RATE_LIMITED") {
                    // The same ordering with a rate-limit reset in the exit
                    // report, so the daemon's auto-resume path must still run.
                    close_stdout();
                    eprintln!("agy-fixture: rate limit reached, resets at 2099-12-01T00:00:00Z");
                    std::process::exit(7);
                }
                let closing_stdin = text.contains("H4RN_FIXTURE_CLOSE_STDIN");
                let limited_stdin = text.contains("H4RN_FIXTURE_STDIN_RATE_LIMITED");
                // Both exit-after-turn orderings hold their answer until the
                // test opens a prompt gate: a message queued behind this turn
                // must be queued while this turn is still active, whatever the
                // machine's scheduling.
                if text.contains("H4RN_FIXTURE_LIMIT_EXIT_AFTER_TURN") {
                    wait_for_gate(
                        ".agy-fixture-limit-after-turn-prompt-waiting",
                        ".agy-fixture-limit-after-turn-prompt-gate",
                    );
                }
                if text.contains("H4RN_FIXTURE_LIMIT_EXIT_AFTER_QUEUE") {
                    wait_for_gate(
                        ".agy-fixture-limit-after-queue-prompt-waiting",
                        ".agy-fixture-limit-after-queue-prompt-gate",
                    );
                }
                let session_id = params
                    .get("sessionId")
                    .and_then(Value::as_str)
                    .unwrap_or("fixture-session");
                write_frame(
                    &mut stdout,
                    &json!({
                        "jsonrpc": "2.0",
                        "method": "session/update",
                        "params": {
                            "sessionId": session_id,
                            "update": {
                                "sessionUpdate": "agent_message_chunk",
                                "content": {"type": "text", "text": format!("echo: {text}")}
                            }
                        }
                    }),
                );
                reply(
                    &mut stdout,
                    id,
                    json!({
                        "stopReason": "end_turn",
                        "usage": {"inputTokens": 3, "outputTokens": 2}
                    }),
                );
                if limited_stdin {
                    // Answer this turn, then leave a rate-limit reset on
                    // stderr and close stdin while staying alive: the next
                    // prompt's write fails before it reaches the agent, and
                    // stderr is the only place the reset appears.
                    close_stdin_and_wait(Some(
                        "agy-fixture: rate limit reached, resets at 2099-12-01T00:00:00Z",
                    ));
                }
                if closing_stdin {
                    // Answer this turn, then shut this process's own stdin and
                    // stay alive. The next prompt is never read: its write
                    // fails on the closed pipe while the agent is still
                    // running, which is the case the daemon's one failure row
                    // must describe with both the I/O error and stderr.
                    close_stdin_and_wait(None);
                }
                if text.contains("H4RN_FIXTURE_LIMIT_EXIT_AFTER_TURN") {
                    // Answer this turn with an ordinary newline-terminated
                    // frame and then exit at once with the reset on stderr. A
                    // real peer does not close stdout first and wait: its exit
                    // closes the pipe, so the process can be reaped before the
                    // reader has drained the reply that is already in it. The
                    // reply must still be the turn's result, and the requeue of
                    // anything the completion promoted is what keeps the pause
                    // authoritative.
                    eprintln!("agy-fixture: rate limit reached, resets at 2099-12-01T00:00:00Z");
                    std::process::exit(7);
                }
                if text.contains("H4RN_FIXTURE_LIMIT_EXIT_AFTER_QUEUE") {
                    // The same death, but the transport stays healthy until the
                    // prompt queued behind this turn reaches it: this process
                    // reads that promoted prompt (writing a marker so the test
                    // knows it arrived), acts on it with an edit it never
                    // reports as a tool call, and only then closes stdout and
                    // exits with the reset. That fixes the hard ordering: the
                    // daemon promotes the queued turn while the transport is
                    // open, so the exit-only limit lands after the promoted
                    // write.
                    wait_for_more_prompts(".agy-fixture-limit-after-queue-second");
                    let _ = std::fs::write(
                        std::path::Path::new(".agy-fixture-limit-after-queue-edit"),
                        b"edited",
                    );
                    wait_for_gate(
                        ".agy-fixture-limit-after-queue-waiting",
                        ".agy-fixture-limit-after-queue-gate",
                    );
                    close_stdout();
                    eprintln!("agy-fixture: rate limit reached, resets at 2099-12-01T00:00:00Z");
                    std::process::exit(7);
                }
                if text.contains("H4RN_FIXTURE_LIMIT_EXIT_BETWEEN_TURNS") {
                    // Answer this turn while the transport is healthy, then
                    // let the test end the turn and only afterwards close
                    // stdout: the idle death's exit report carries the reset.
                    wait_for_gate(
                        ".agy-fixture-limit-idle-waiting",
                        ".agy-fixture-limit-idle-gate",
                    );
                    close_stdout();
                    // No pause here: the report follows the EOF at once, so the
                    // pump's bounded wait for the death's specific account can
                    // never expire before it lands. A pause would only make the
                    // test depend on the loaded machine's scheduling.
                    eprintln!("agy-fixture: rate limit reached, resets at 2099-12-01T00:00:00Z");
                    std::process::exit(7);
                }
                if text.contains("H4RN_FIXTURE_IDLE_EXIT_ONLY") {
                    // Answer this turn, wait for the test to end it, then die
                    // with only the process exit to report it: a keeper process
                    // inherits stdout and holds its pipe open, so the daemon's
                    // reader never sees EOF and cannot publish the generic close
                    // notice. The reset on stderr makes the exit report a typed
                    // limit, which is the ordering that used to leave the dead
                    // session attached until the next Send failed on it.
                    wait_for_gate(
                        ".agy-fixture-idle-exit-waiting",
                        ".agy-fixture-idle-exit-gate",
                    );
                    let _keeper = Command::new("sleep")
                        .arg("5")
                        .stdin(Stdio::null())
                        .stderr(Stdio::null())
                        .spawn()
                        .unwrap_or_else(|error| {
                            eprintln!(
                                "agy-fixture: could not start the stdout keeper process: {error}"
                            );
                            std::process::exit(3);
                        });
                    eprintln!("agy-fixture: rate limit reached, resets at 2099-12-01T00:00:00Z");
                    std::process::exit(7);
                }
                if text.contains("H4RN_FIXTURE_DIE_BETWEEN_TURNS") {
                    // Answer this turn, then leave the session idle and only
                    // then exit: the death happens between turns, and the
                    // daemon's row for it must arrive without another prompt.
                    // The test releases this gate after it has seen the turn
                    // end, so the ordering is not a sleep's guess.
                    wait_for_gate(
                        ".agy-fixture-between-turns-waiting",
                        ".agy-fixture-between-turns-gate",
                    );
                    eprintln!("agy-fixture: the session ended between turns");
                    std::process::exit(7);
                }
                if text.contains("H4RN_FIXTURE_STALL_STDIN") {
                    // Answer this turn, print the provider's reset, then stop
                    // reading stdin without closing it. The next prompt is
                    // larger than the pipe can buffer, so its write stalls
                    // until the adapter's deadline kills this process; the
                    // reset printed here is the only place the limit appears.
                    eprintln!("agy-fixture: rate limit reached, resets at 2099-12-01T00:00:00Z");
                    stall_forever();
                }
            }
            "session/cancel" => {
                if let Some(prompt_id) = pending_prompt.take() {
                    reply(
                        &mut stdout,
                        Some(prompt_id),
                        json!({"stopReason": "cancelled"}),
                    );
                }
            }
            other => {
                if let Some(id) = id {
                    write_frame(
                        &mut stdout,
                        &json!({
                            "jsonrpc": "2.0",
                            "id": id,
                            "error": {"code": -32601, "message": format!("method not found: {other}")}
                        }),
                    );
                }
            }
        }
    }
}

/// Announces that the dying turn has arrived, then waits for the test's gate.
///
/// The marker is written first so the test can queue a follow-up behind the
/// turn, and only then create the gate the process is waiting for.
fn wait_for_exit_gate() {
    wait_for_gate(".agy-fixture-exit-waiting", ".agy-fixture-exit-gate");
}

/// Waits, bounded, for the next prompt to reach this process, then marks it.
///
/// The read loop is single-threaded (closing this process's stdin while another
/// thread blocks in it would leave the pipe's read end open), so the handler
/// polls stdin itself: the daemon writes the next prompt only after this turn's
/// reply, which the main loop has already consumed, so no frame can be sitting
/// in the reader's buffer. The marker is what proves the daemon delivered that
/// prompt before the transport died.
fn wait_for_more_prompts(marker: &str) {
    let Ok(cwd) = std::env::current_dir() else {
        return;
    };
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    let mut buffered = Vec::new();
    while let Some(left) = deadline.checked_duration_since(std::time::Instant::now()) {
        let Some(chunk) = poll_stdin(left) else {
            return;
        };
        buffered.extend_from_slice(&chunk);
        while let Some(end) = buffered.iter().position(|byte| *byte == b'\n') {
            let line: Vec<u8> = buffered.drain(..=end).collect();
            let Ok(frame) = serde_json::from_slice::<Value>(&line) else {
                continue;
            };
            if frame.get("method").and_then(Value::as_str) == Some("session/prompt") {
                let _ = std::fs::write(cwd.join(marker), b"");
                return;
            }
        }
    }
}

/// One bounded read from stdin, or `None` when it ended or waited too long.
#[cfg(unix)]
fn poll_stdin(timeout: std::time::Duration) -> Option<Vec<u8>> {
    let millis = timeout.as_millis().min(i32::MAX as u128) as i32;
    let mut fd = libc::pollfd {
        fd: 0,
        events: libc::POLLIN,
        revents: 0,
    };
    // SAFETY: `fd` is one valid pollfd for this thread-owned stack slot, and
    // fd 0 is this process's stdin.
    if unsafe { libc::poll(&mut fd, 1, millis) } <= 0 {
        return None;
    }
    let mut chunk = [0u8; 4096];
    // SAFETY: `chunk` is a valid writable buffer of the given length.
    let read = unsafe { libc::read(0, chunk.as_mut_ptr().cast(), chunk.len()) };
    if read <= 0 {
        return None;
    }
    Some(chunk[..read as usize].to_vec())
}

/// This ordering is unix-only; other platforms never run its test.
#[cfg(not(unix))]
fn poll_stdin(_: std::time::Duration) -> Option<Vec<u8>> {
    None
}

/// Writes `waiting` in the session workspace, then waits until `gate` appears.
///
/// Both names are relative to the workspace, which the daemon starts the agent
/// in. The wait is bounded so a test that never creates the gate fails the turn
/// instead of hanging the fixture forever.
fn wait_for_gate(waiting: &str, gate: &str) {
    let Ok(cwd) = std::env::current_dir() else {
        return;
    };
    let _ = std::fs::write(cwd.join(waiting), b"");
    let gate = cwd.join(gate);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    while !gate.exists() && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}

/// Stops reading stdin while keeping the process, and its stdin, alive.
///
/// A prompt larger than the pipe can buffer then never flushes, which is the
/// stall the adapter's write deadline exists for.
fn stall_forever() -> ! {
    loop {
        std::thread::sleep(std::time::Duration::from_secs(1));
    }
}

/// Writes the diagnostic a test checks, then closes this process's stdin.
///
/// The diagnostic and the close are one step: the daemon's next prompt write
/// fails on the closed pipe, and the row it publishes must wait for this line
/// to reach the client's stderr buffer instead of assuming a handshake-time
/// line.
fn close_stdin_and_wait(diagnostic: Option<&str>) -> ! {
    match diagnostic {
        Some(line) => eprintln!("{line}"),
        None => eprintln!("agy-fixture: closing stdin before the next prompt"),
    }
    #[cfg(unix)]
    {
        // SAFETY: closing this process's own stdin descriptor is the point of
        // the fixture, and no later code reads it.
        unsafe {
            libc::close(0);
        }
    }
    if let Ok(cwd) = std::env::current_dir() {
        let _ = std::fs::write(cwd.join(".agy-fixture-stdin-closed"), b"");
    }
    loop {
        std::thread::sleep(std::time::Duration::from_secs(1));
    }
}

/// Closes this process's own stdout, leaving stdin and stderr open.
///
/// The daemon's reader sees EOF at once, before the process has exited, which
/// is the race the exit report's bounded wait exists for.
fn close_stdout() {
    #[cfg(unix)]
    {
        // SAFETY: closing this process's own stdout descriptor is the point of
        // the fixture.
        unsafe {
            libc::close(1);
        }
    }
}

fn reply(stdout: &mut std::io::Stdout, id: Option<Value>, result: Value) {
    if let Some(id) = id {
        write_frame(
            stdout,
            &json!({"jsonrpc": "2.0", "id": id, "result": result}),
        );
    }
}

fn session_reply(session_id: &str) -> Value {
    // A dual-catalogue agent publishes both the legacy `models`/`modes`
    // objects and ACP v1 `configOptions`, with different ids. The generic
    // adapter must read and select the v1 catalogue.
    if std::env::var_os("H4RN_FIXTURE_DUAL_CATALOGUE").is_some() {
        return json!({
            "sessionId": session_id,
            "models": {
                "availableModels": [{"modelId": "legacy-model", "name": "Legacy"}],
                "currentModelId": "legacy-model"
            },
            "modes": {
                "availableModes": [
                    {"id": "legacy-mode", "name": "Legacy mode"}
                ],
                "currentModeId": "legacy-mode"
            },
            "configOptions": [
                {
                    "id": "model",
                    "name": "Model",
                    "category": "model",
                    "type": "select",
                    "currentValue": "v1-model",
                    "options": [{"value": "v1-model", "name": "V1 Model"}]
                },
                {
                    "id": "mode",
                    "name": "Mode",
                    "category": "mode",
                    "type": "select",
                    "currentValue": "default",
                    "options": [{"value": "default", "name": "Default"}]
                }
            ]
        });
    }
    json!({
        "sessionId": session_id,
        "models": {
            "availableModels": [{"modelId": "fixture-model", "name": "Fixture"}],
            "currentModelId": "fixture-model"
        },
        "modes": {
            "availableModes": [{"id": "default", "name": "Default"}],
            "currentModeId": "default"
        }
    })
}

/// Records that a process opened a session, so a test can prove the daemon
/// started a fresh one after a dead transport.
fn log_start(workspace: &str, kind: &str) {
    let path = std::path::Path::new(workspace).join(".agy-fixture-sessions");
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    {
        let _ = writeln!(file, "{kind}");
    }
}

fn write_frame(stdout: &mut std::io::Stdout, frame: &Value) {
    let Ok(mut bytes) = serde_json::to_vec(frame) else {
        return;
    };
    bytes.push(b'\n');
    let _ = stdout.write_all(&bytes);
    let _ = stdout.flush();
}
