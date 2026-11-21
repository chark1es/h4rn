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
//! the exit (`H4RN_FIXTURE_LIMIT_EXIT_BETWEEN_TURNS`). A mid-turn exit with no
//! stderr at all (`H4RN_FIXTURE_SILENT_EXIT`) proves the exit status still names
//! the row when there is no stderr to outrank it. An idle death with no EOF at
//! all (`H4RN_FIXTURE_IDLE_EXIT_ONLY`) holds stdout open from a keeper process,
//! so only the exit report can announce it.

use std::io::{BufRead, Write};
use std::process::{Command, Stdio};

use serde_json::{Value, json};

fn main() {
    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout();
    let mut sessions = 0u64;

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
            // A response to a request this fixture never sends; nothing to do.
            continue;
        };
        let params = frame.get("params").cloned().unwrap_or(Value::Null);

        match method {
            "initialize" => reply(
                &mut stdout,
                id,
                json!({
                    "protocolVersion": 1,
                    "agentInfo": {"name": "agy fixture", "version": env!("CARGO_PKG_VERSION")},
                    "agentCapabilities": {
                        "loadSession": true,
                        "promptCapabilities": {"image": false, "embeddedContext": false},
                        "mcpCapabilities": {},
                        "sessionCapabilities": {"load": {}, "close": {}}
                    }
                }),
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
                reply(&mut stdout, id, session_reply(&session_id));
            }
            "session/set_model" | "session/set_mode" | "session/close" => {
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
                    // stderr and exit. The daemon wakes on the EOF before the
                    // process is reaped, so its failure row can carry the line
                    // only through the exit report.
                    close_stdout();
                    // Leave the daemon's reader to wake on the EOF first: the
                    // diagnostic is written after a pause, so only the exit
                    // report can carry it.
                    std::thread::sleep(std::time::Duration::from_millis(150));
                    eprintln!("agy-fixture: the session died after stdout closed");
                    std::process::exit(7);
                }
                if text.contains("H4RN_FIXTURE_SILENT_EXIT") {
                    // Close stdout and exit with no stderr at all: the row can
                    // only name the exit status, which must beat the generic
                    // stdout-close notice.
                    close_stdout();
                    std::thread::sleep(std::time::Duration::from_millis(150));
                    std::process::exit(7);
                }
                if text.contains("H4RN_FIXTURE_RATE_LIMITED") {
                    // The same ordering with a rate-limit reset in the exit
                    // report, so the daemon's auto-resume path must still run.
                    close_stdout();
                    std::thread::sleep(std::time::Duration::from_millis(150));
                    eprintln!("agy-fixture: rate limit reached, resets at 2099-12-01T00:00:00Z");
                    std::process::exit(7);
                }
                let closing_stdin = text.contains("H4RN_FIXTURE_CLOSE_STDIN");
                let limited_stdin = text.contains("H4RN_FIXTURE_STDIN_RATE_LIMITED");
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
                    // Answer this turn, close stdout at once, and only then
                    // leave the reset on stderr and exit. The turn's completion
                    // sees a closed transport, and the limit arrives solely
                    // through the exit report that follows. The gate marks that
                    // the answer was written and stdout closed, so a test can
                    // queue work behind the turn before the report lands.
                    close_stdout();
                    wait_for_gate(
                        ".agy-fixture-limit-after-turn-waiting",
                        ".agy-fixture-limit-after-turn-gate",
                    );
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
                    std::thread::sleep(std::time::Duration::from_millis(150));
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
                        .spawn();
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
            "session/cancel" => {}
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
