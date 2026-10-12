//! End to end: `sun plugin mcp add` registers a URL MCP server (connecting
//! once), then `sun mcp proxy <grant>` (what a session's MCP config runs)
//! serves only the granted tools on stdio, and refuses an unregistered one.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Command, Stdio};

use axum::Router;
use axum::http::{StatusCode, header};
use axum::routing::post;
use serde_json::{Value, json};

/// A two-tool MCP server over plain JSON HTTP: `probe` (read-only) and
/// `write_note`.
fn reply(message: &Value) -> Option<Value> {
    let id = message.get("id")?.clone();
    let result = match message["method"].as_str()? {
        "initialize" => json!({"protocolVersion": message["params"]["protocolVersion"],
            "capabilities": {"tools": {}}, "serverInfo": {"name": "echo", "version": "0"}}),
        "tools/list" => json!({"tools": [
            {"name": "probe", "inputSchema": {"type": "object"},
             "annotations": {"readOnlyHint": true}},
            {"name": "write_note", "inputSchema": {"type": "object"}},
        ]}),
        _ => json!({}),
    };
    Some(json!({"jsonrpc": "2.0", "id": id, "result": result}))
}

fn sun(data_dir: &Path, args: &[&str]) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_sun"));
    command.arg("--data-dir").arg(data_dir).args(args);
    command
}

#[test]
fn proxy_serves_only_the_granted_tools_of_a_registered_server() {
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let url = runtime.block_on(async {
        let app = Router::new().route(
            "/mcp",
            post(|body: String| async move {
                let message: Value = serde_json::from_str(&body).unwrap_or_default();
                let json = [(header::CONTENT_TYPE, "application/json")];
                match reply(&message) {
                    Some(r) => (StatusCode::OK, json, r.to_string()),
                    None => (StatusCode::ACCEPTED, json, String::new()),
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/mcp", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await });
        url
    });
    let data = tempfile::tempdir().unwrap();

    let added = sun(
        data.path(),
        &["plugin", "mcp", "add", "docs", "--url", &url],
    )
    .output()
    .unwrap();
    let stdout = String::from_utf8_lossy(&added.stdout);
    assert!(
        added.status.success(),
        "{stdout}{}",
        String::from_utf8_lossy(&added.stderr)
    );
    assert!(
        stdout.contains("probe") && stdout.contains("read-only"),
        "{stdout}"
    );
    assert!(stdout.contains("write_note"), "{stdout}");

    let refused = sun(data.path(), &["mcp", "proxy", r#"{"server":"nope"}"#])
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(!refused.status.success());
    assert!(String::from_utf8_lossy(&refused.stderr).contains("not registered"));

    let mut proxy = sun(data.path(), &["mcp", "proxy", r#"{"server":"docs"}"#])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdin = proxy.stdin.take().unwrap();
    let mut lines = BufReader::new(proxy.stdout.take().unwrap()).lines();
    let mut send = |message: Value| writeln!(stdin, "{message}").unwrap();
    send(
        json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {
        "protocolVersion": "2025-06-18", "capabilities": {},
        "clientInfo": {"name": "agent", "version": "0"}}}),
    );
    let initialized: Value = serde_json::from_str(&lines.next().unwrap().unwrap()).unwrap();
    assert_eq!(initialized["id"], 1, "{initialized}");
    send(json!({"jsonrpc": "2.0", "method": "notifications/initialized"}));
    send(json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"}));
    let listed: Value = serde_json::from_str(&lines.next().unwrap().unwrap()).unwrap();
    let names: Vec<&str> = listed["result"]["tools"]
        .as_array()
        .expect("tools")
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["probe"], "the default grant is read-only tools");
    let _ = proxy.kill();
    let _ = proxy.wait();
}
