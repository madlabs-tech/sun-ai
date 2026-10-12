//! A tiny MCP server for tests: newline-delimited JSON-RPC on
//! stdio. `tests/mcp.rs` also serves [`handle`] over HTTP.
//!
//! Tools: `probe` (read-only) reports `HOME`, the `SUN_TEST_TOKEN` env var
//! and whether it could read `path`; `write_note` (unmarked, so not
//! read-only) just answers `written`.

use std::io::{BufRead, Write};

use serde_json::{Value, json};

#[allow(dead_code)] // the HTTP test server uses only `handle`
fn main() {
    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout();
    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        let Ok(message) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        if let Some(reply) = handle(&message) {
            let _ = writeln!(stdout, "{reply}");
            let _ = stdout.flush();
        }
    }
}

/// The reply to one JSON-RPC message; `None` for notifications.
pub fn handle(message: &Value) -> Option<Value> {
    let id = message.get("id")?.clone();
    let params = &message["params"];
    let result = match message["method"].as_str().unwrap_or_default() {
        "initialize" => json!({
            "protocolVersion": params["protocolVersion"],
            "capabilities": {"tools": {}},
            "serverInfo": {"name": "echo-mcp", "version": "0.1.0"},
        }),
        "ping" => json!({}),
        "tools/list" => json!({"tools": [
            {"name": "probe", "description": "Reports its environment.",
             "inputSchema": {"type": "object", "properties": {"path": {"type": "string"}}},
             "annotations": {"readOnlyHint": true}},
            {"name": "write_note", "description": "Writes a note.",
             "inputSchema": {"type": "object"}},
        ]}),
        "tools/call" => {
            let text = match params["name"].as_str() {
                Some("probe") => {
                    let path = params["arguments"]["path"]
                        .as_str()
                        .unwrap_or("/nonexistent");
                    json!({
                        "home": std::env::var("HOME").ok(),
                        "token": std::env::var("SUN_TEST_TOKEN").ok(),
                        "read": std::fs::read(path).is_ok(),
                    })
                    .to_string()
                }
                Some("write_note") => "written".to_string(),
                _ => {
                    return Some(json!({"jsonrpc": "2.0", "id": id,
                        "error": {"code": -32602, "message": "unknown tool"}}));
                }
            };
            json!({"content": [{"type": "text", "text": text}], "isError": false})
        }
        _ => {
            return Some(json!({"jsonrpc": "2.0", "id": id,
                "error": {"code": -32601, "message": "method not found"}}));
        }
    };
    Some(json!({"jsonrpc": "2.0", "id": id, "result": result}))
}
