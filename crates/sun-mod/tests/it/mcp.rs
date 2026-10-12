//! End-to-end MCP connectors: the tiny MCP server
//! (`tests/fixtures/echo_mcp.rs`) registered by command (sandboxed) and by
//! URL (an in-test HTTP server), its tools listed, and an agent session
//! seeing only what the task's grant enables.
#![cfg(any(target_os = "macos", target_os = "linux"))]
#![allow(clippy::unwrap_used, clippy::expect_used)] // test helpers

#[allow(dead_code)]
#[path = "../fixtures/echo_mcp.rs"]
mod echo_mcp;

use std::path::{Path, PathBuf};

use rmcp::model::{CallToolRequestParams, CallToolResult};
use rmcp::service::RunningService;
use rmcp::{RoleClient, ServiceExt};
use serde_json::{Value, json};
use sun_mod::{McpClient, McpGrant, McpServer, McpTool, McpTransport, serve_mcp_proxy};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

fn temp_root(name: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!("sun-mcp-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    root
}

/// An agent session connected to [`serve_mcp_proxy`] over an in-memory pipe.
async fn agent_via_proxy(upstream: McpClient, grant: McpGrant) -> RunningService<RoleClient, ()> {
    let (agent, kernel) = tokio::io::duplex(64 * 1024);
    let (kernel_read, kernel_write) = tokio::io::split(kernel);
    tokio::spawn(serve_mcp_proxy(upstream, grant, kernel_read, kernel_write));
    ().serve(tokio::io::split(agent)).await.unwrap()
}

async fn call(agent: &RunningService<RoleClient, ()>, tool: &str, args: Value) -> CallToolResult {
    let mut params = CallToolRequestParams::new(tool.to_string());
    params.arguments = args.as_object().cloned();
    agent.call_tool(params).await.unwrap()
}

fn text(result: &CallToolResult) -> String {
    let content = serde_json::to_value(&result.content).unwrap();
    content[0]["text"].as_str().unwrap_or_default().to_string()
}

async fn tool_names(agent: &RunningService<RoleClient, ()>) -> Vec<String> {
    let tools = agent.list_all_tools().await.unwrap();
    tools.iter().map(|t| t.name.to_string()).collect()
}

fn expected_tools() -> Vec<McpTool> {
    vec![
        McpTool {
            name: "probe".into(),
            description: "Reports its environment.".into(),
            read_only: true,
        },
        McpTool {
            name: "write_note".into(),
            description: "Writes a note.".into(),
            read_only: false,
        },
    ]
}

/// Linux: `Sandbox::for_mod` looks for the launcher next to the current
/// executable, which for a test is `target/<profile>/deps/`.
fn install_launcher() {
    #[cfg(target_os = "linux")]
    {
        static ONCE: std::sync::Once = std::sync::Once::new();
        ONCE.call_once(|| {
            let exe = std::env::current_exe().unwrap();
            let target = exe.parent().unwrap().join("sun-mod-sandbox");
            std::fs::copy(env!("CARGO_BIN_EXE_sun-mod-sandbox"), target).unwrap();
        });
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn mcp_command_server_runs_sandboxed_behind_a_read_only_proxy() {
    install_launcher();
    let root = temp_root("command");
    let bin = root.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    std::fs::copy(env!("CARGO_BIN_EXE_sun-echo-mcp"), bin.join("echo-mcp")).unwrap();
    let secret_file = root.join("secret.txt");
    std::fs::write(&secret_file, "kernel secret").unwrap();
    let server = McpServer {
        name: "echo".into(),
        transport: McpTransport::Command {
            command: bin.join("echo-mcp").display().to_string(),
            args: Vec::new(),
            network: Vec::new(),
            secrets: vec!["SUN_TEST_TOKEN".into()],
        },
    };
    let data_dir = root.join("data");
    let secrets = vec![("SUN_TEST_TOKEN".to_string(), "granted".to_string())];
    let client = McpClient::connect(&server, &data_dir, secrets.clone())
        .await
        .unwrap();
    assert_eq!(client.tools().await.unwrap(), expected_tools());

    // Default grant: only the read-only tool is visible or callable.
    let client = McpClient::connect(&server, &data_dir, secrets)
        .await
        .unwrap();
    let grant = McpGrant {
        server: "echo".into(),
        tools: None,
    };
    let agent = agent_via_proxy(client, grant).await;
    assert_eq!(tool_names(&agent).await, ["probe"]);
    let refused = call(&agent, "write_note", json!({})).await;
    assert_eq!(refused.is_error, Some(true));
    assert!(text(&refused).contains("not enabled"), "{}", text(&refused));

    // The server ran inside the sandbox: its HOME is the data dir, it got
    // the granted secret, and it can't read files outside its folders.
    let probe = call(&agent, "probe", json!({"path": secret_file})).await;
    let probe: Value = serde_json::from_str(&text(&probe)).unwrap();
    let home = PathBuf::from(probe["home"].as_str().unwrap());
    assert_eq!(
        home.canonicalize().unwrap(),
        data_dir.canonicalize().unwrap()
    );
    assert_eq!(probe["token"], "granted");
    assert_eq!(
        probe["read"], false,
        "sandbox let the server read {secret_file:?}"
    );
    agent.cancel().await.unwrap();
}

/// A minimal streamable-HTTP MCP endpoint serving [`echo_mcp::handle`],
/// one request per connection, requiring `Authorization: Bearer <token>`.
async fn http_server(token: &'static str) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/mcp", listener.local_addr().unwrap());
    tokio::spawn(async move {
        while let Ok((mut stream, _)) = listener.accept().await {
            tokio::spawn(async move {
                let (head, body) = read_request(&mut stream).await;
                let authorized = head
                    .lines()
                    .any(|l| l.eq_ignore_ascii_case(&format!("authorization: Bearer {token}")));
                let (status, reply) = match () {
                    _ if !authorized => ("401 Unauthorized", None),
                    _ if !head.starts_with("POST ") => ("405 Method Not Allowed", None),
                    _ => match serde_json::from_slice(&body)
                        .ok()
                        .and_then(|m| echo_mcp::handle(&m))
                    {
                        Some(reply) => ("200 OK", Some(reply.to_string())),
                        None => ("202 Accepted", None),
                    },
                };
                let body = reply.unwrap_or_default();
                let response = format!(
                    "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = stream.write_all(response.as_bytes()).await;
            });
        }
    });
    url
}

/// Reads one HTTP/1.1 request: the head as text and the body.
async fn read_request(stream: &mut tokio::net::TcpStream) -> (String, Vec<u8>) {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    let head_end = loop {
        if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break i + 4;
        }
        let n = stream.read(&mut chunk).await.unwrap();
        if n == 0 {
            return (String::new(), Vec::new());
        }
        buf.extend_from_slice(&chunk[..n]);
    };
    let head = String::from_utf8_lossy(&buf[..head_end]).to_string();
    let length = head
        .lines()
        .find_map(|l| {
            let (name, value) = l.split_once(':')?;
            name.eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse::<usize>().ok())?
        })
        .unwrap_or(0);
    while buf.len() < head_end + length {
        let n = stream.read(&mut chunk).await.unwrap();
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..n]);
    }
    (head, buf[head_end..].to_vec())
}

#[tokio::test(flavor = "multi_thread")]
async fn mcp_url_server_uses_the_bearer_secret_and_explicit_grants() {
    let url = http_server("t0k").await;
    let server = McpServer {
        name: "remote".into(),
        transport: McpTransport::Url {
            url,
            bearer_secret: Some("REMOTE_TOKEN".into()),
        },
    };
    let data_dir: &Path = &temp_root("url");
    let wrong = vec![("REMOTE_TOKEN".to_string(), "nope".to_string())];
    assert!(McpClient::connect(&server, data_dir, wrong).await.is_err());
    let secrets = vec![("REMOTE_TOKEN".to_string(), "t0k".to_string())];
    let client = McpClient::connect(&server, data_dir, secrets.clone())
        .await
        .unwrap();
    assert_eq!(client.tools().await.unwrap(), expected_tools());

    // An explicit tool list replaces the read-only default.
    let client = McpClient::connect(&server, data_dir, secrets)
        .await
        .unwrap();
    let grant = McpGrant {
        server: "remote".into(),
        tools: Some(vec!["write_note".into()]),
    };
    let agent = agent_via_proxy(client, grant).await;
    assert_eq!(tool_names(&agent).await, ["write_note"]);
    assert_eq!(
        text(&call(&agent, "write_note", json!({})).await),
        "written"
    );
    let refused = call(&agent, "probe", json!({})).await;
    assert_eq!(refused.is_error, Some(true));
    agent.cancel().await.unwrap();
}

/// One plugin is the whole connector. A mod with `mcp_tools` serves
/// tools to a granted agent session through the same filtering proxy, and
/// the same process emits the trigger events that start workflows.
#[tokio::test(flavor = "multi_thread")]
async fn granted_session_lists_mod_tool() {
    use std::sync::Arc;
    use sun_core::ModLink;

    let yaml = format!(
        "name: echo\nkind: plugin\nversion: 0.1.0\napi: '>=1.0 <2.0'\n\
         entry: {{ command: \"{}\" }}\n\
         provides: {{ actions: [echo], triggers: [tick], mcp_tools: true }}\n",
        env!("CARGO_BIN_EXE_sun-echo-mod")
    );
    let host = Arc::new(sun_mod::ModHost::new(sun_mod::HostConfig::default()));
    host.add(
        sun_mod::parse_manifest(&yaml).unwrap(),
        std::env::temp_dir(),
    )
    .await
    .unwrap();
    let link: Arc<dyn ModLink> = host.clone();

    let client = McpClient::for_mod(link.clone(), "echo");
    let listed = client.tools().await.unwrap();
    assert_eq!(listed.len(), 2, "{listed:?}");
    assert!(listed.iter().any(|t| t.name == "lookup" && t.read_only));

    // The default grant shows only the read-only tool, and calls reach the mod.
    let grant = McpGrant {
        server: "echo".into(),
        tools: None,
    };
    let agent = agent_via_proxy(McpClient::for_mod(link.clone(), "echo"), grant).await;
    assert_eq!(tool_names(&agent).await, ["lookup"]);
    let found = call(&agent, "lookup", json!({"word": "sun"})).await;
    assert_eq!(text(&found), "lookup: sun");
    let refused = call(&agent, "post", json!({})).await;
    assert_eq!(refused.is_error, Some(true));

    // The same connector emits events.
    let mut events = link.events();
    link.subscribe("echo", vec!["tick".into()]).await.unwrap();
    let poll = std::future::poll_fn(|cx| {
        futures_core::Stream::poll_next(std::pin::Pin::new(&mut events), cx)
    });
    let event = tokio::time::timeout(std::time::Duration::from_secs(5), poll)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        (event.mod_name.as_str(), event.topic.as_str()),
        ("echo", "tick")
    );

    // A plugin without `mcp_tools` serves none.
    let plain = yaml
        .replace(", mcp_tools: true", "")
        .replace("name: echo", "name: plain");
    host.add(
        sun_mod::parse_manifest(&plain).unwrap(),
        std::env::temp_dir(),
    )
    .await
    .unwrap();
    assert!(McpClient::for_mod(link, "plain").tools().await.is_err());
    agent.cancel().await.unwrap();
    host.shutdown().await;
}
