//! A minimal ACP agent used by daemon integration tests.
//!
//! It speaks just enough of the protocol for the Antigravity adapter: the
//! handshake, a session, one echo turn per prompt. A prompt containing
//! `H4RN_FIXTURE_EXIT` makes the process vanish mid-turn, which is how the
//! tests drive a dead transport without a real `agy` install.

use std::io::{BufRead, Write};

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
                    // dead transport and never gets a prompt result.
                    std::process::exit(7);
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
