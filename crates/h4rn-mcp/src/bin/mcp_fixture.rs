//! A tiny stdio MCP server used by integration tests.
//!
//! It is intentionally dependency-free and synchronous: the hub starts it as a
//! real child process, so the test exercises the same spawn, handshake and
//! line-framing path a user-configured server would. It exposes enough surface
//! to cover namespacing: one echo tool, one failing tool, one resource and one
//! prompt.

use std::io::{BufRead, Write};

use serde_json::{Value, json};

fn main() {
    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout().lock();
    for line in stdin.lock().lines() {
        let Ok(line) = line else {
            break;
        };
        if line.trim().is_empty() {
            continue;
        }
        let Ok(message) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        if let Some(response) = handle(&message) {
            let _ = writeln!(stdout, "{response}");
            let _ = stdout.flush();
        }
    }
}

fn handle(message: &Value) -> Option<Value> {
    let id = message.get("id").cloned()?;
    let method = message.get("method").and_then(Value::as_str)?;
    let params = message.get("params").cloned().unwrap_or(Value::Null);
    let result = match method {
        "initialize" => {
            let requested = params
                .get("protocolVersion")
                .and_then(Value::as_str)
                .unwrap_or("2025-06-18");
            json!({
                "protocolVersion": requested,
                "capabilities": {
                    "tools": {"listChanged": false},
                    "resources": {"listChanged": false},
                    "prompts": {"listChanged": false}
                },
                "serverInfo": {"name": "fixture", "version": "0.1.0"},
                "instructions": "fixture server"
            })
        }
        "ping" => json!({}),
        "tools/list" => json!({
            "tools": [
                {
                    "name": "echo",
                    "description": "Echo the message back.",
                    "inputSchema": {
                        "type": "object",
                        "properties": {"message": {"type": "string"}},
                        "required": ["message"]
                    }
                },
                {
                    "name": "fail",
                    "description": "Always report a tool error.",
                    "inputSchema": {"type": "object", "properties": {}}
                }
            ]
        }),
        "tools/call" => {
            let name = params.get("name").and_then(Value::as_str).unwrap_or("");
            match name {
                "echo" => {
                    let message = params
                        .get("arguments")
                        .and_then(|arguments| arguments.get("message"))
                        .and_then(Value::as_str)
                        .unwrap_or("");
                    json!({
                        "content": [{"type": "text", "text": message}],
                        "isError": false
                    })
                }
                "fail" => json!({
                    "content": [{"type": "text", "text": "the fixture failed on purpose"}],
                    "isError": true
                }),
                other => {
                    return Some(error(id, -32602, format!("unknown tool \"{other}\"")));
                }
            }
        }
        "resources/list" => json!({
            "resources": [{
                "uri": "fixture://greeting",
                "name": "greeting",
                "description": "A fixed greeting.",
                "mimeType": "text/plain"
            }]
        }),
        "resources/read" => {
            let uri = params.get("uri").and_then(Value::as_str).unwrap_or("");
            if uri != "fixture://greeting" {
                return Some(error(id, -32002, format!("unknown resource \"{uri}\"")));
            }
            json!({
                "contents": [{
                    "uri": uri,
                    "mimeType": "text/plain",
                    "text": "hello from the fixture"
                }]
            })
        }
        "prompts/list" => json!({
            "prompts": [{
                "name": "greet",
                "description": "Greet someone.",
                "arguments": [{"name": "who", "required": false}]
            }]
        }),
        "prompts/get" => {
            let who = params
                .get("arguments")
                .and_then(|arguments| arguments.get("who"))
                .and_then(Value::as_str)
                .unwrap_or("world");
            json!({
                "description": "Greet someone.",
                "messages": [{
                    "role": "user",
                    "content": {"type": "text", "text": format!("hello {who}")}
                }]
            })
        }
        "notifications/initialized" | "notifications/cancelled" => return None,
        other => return Some(error(id, -32601, format!("no method \"{other}\""))),
    };
    Some(json!({"jsonrpc": "2.0", "id": id, "result": result}))
}

fn error(id: Value, code: i64, message: impl Into<String>) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": {"code": code, "message": message.into()}
    })
}
