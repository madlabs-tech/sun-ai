//! `/mcp_servers` and `/tasks/{id}/mcp`: register a URL MCP
//! server with an ecosystem bearer secret, list its tools, grant it to a task
//! (and its workflow), then reach it through the kernel's relay
//! (`GET /mcp/{server}`) the way `sun mcp relay` on a remote host does.
#![allow(clippy::unwrap_used)]

use crate::common;

#[allow(dead_code)]
#[path = "../../../sun-mod/tests/fixtures/echo_mcp.rs"]
mod echo_mcp;

use axum::Router;
use axum::body::Body;
use axum::http::{HeaderMap, Request, StatusCode, header};
use axum::routing::post;
use common::{TOKEN, kernel};
use futures_util::{SinkExt, StreamExt};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tower::ServiceExt;

async fn call(app: &Router, method: &str, path: &str, body: Option<Value>) -> (StatusCode, Value) {
    let request = Request::builder()
        .method(method)
        .uri(path)
        .header(header::AUTHORIZATION, format!("Bearer {TOKEN}"))
        .header(header::CONTENT_TYPE, "application/json")
        .body(body.map_or(Body::empty(), |b| Body::from(b.to_string())))
        .unwrap();
    let response = app.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

/// The echo MCP server over plain JSON HTTP, accepting only `Bearer <token>`.
async fn echo_server(token: &'static str) -> String {
    let app = Router::new().route(
        "/mcp",
        post(move |headers: HeaderMap, body: String| async move {
            let auth = headers
                .get(header::AUTHORIZATION)
                .and_then(|v| v.to_str().ok());
            let json = [(header::CONTENT_TYPE, "application/json")];
            if auth != Some(&format!("Bearer {token}")) {
                return (StatusCode::UNAUTHORIZED, json, String::new());
            }
            let message: Value = serde_json::from_str(&body).unwrap_or_default();
            match echo_mcp::handle(&message) {
                Some(reply) => (StatusCode::OK, json, reply.to_string()),
                None => (StatusCode::ACCEPTED, json, String::new()),
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/mcp", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await });
    url
}

#[tokio::test(flavor = "multi_thread")]
async fn mcp_servers_are_tested_on_add_and_granted_per_task() {
    let k = kernel().await;
    let app = &k.app;
    let url = echo_server("t0ken-value").await;
    let docs = json!({"name": "docs", "transport":
        {"type": "url", "url": url, "bearer_secret": "DOCS_TOKEN"}});

    // The bearer secret is not stored yet: plain 422 naming it. Until MCP
    // servers are per workspace their secrets are Main's.
    let (status, error) = call(app, "POST", "/api/v1/mcp_servers", Some(docs.clone())).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{error}");
    assert!(error.to_string().contains("DOCS_TOKEN"), "{error}");
    let (status, _) = call(
        app,
        "PUT",
        "/api/v1/secrets/workspace/00000000000000000000000001/DOCS_TOKEN",
        Some(json!({"value": "t0ken-value"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (status, added) = call(app, "POST", "/api/v1/mcp_servers", Some(docs.clone())).await;
    assert_eq!(status, StatusCode::CREATED, "{added}");
    assert_eq!(added["server"], docs);
    assert_eq!(
        added["tools"],
        json!([
            {"name": "probe", "description": "Reports its environment.", "read_only": true},
            {"name": "write_note", "description": "Writes a note.", "read_only": false},
        ])
    );
    let (status, _) = call(app, "POST", "/api/v1/mcp_servers", Some(docs.clone())).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "names are unique");
    let bad = json!({"name": "sun", "transport": {"type": "url", "url": "https://x"}});
    let (status, _) = call(app, "POST", "/api/v1/mcp_servers", Some(bad)).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);

    let (_, servers) = call(app, "GET", "/api/v1/mcp_servers", None).await;
    assert_eq!(servers, json!([docs]));
    let (status, tools) = call(app, "GET", "/api/v1/mcp_servers/docs/tools", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(tools, added["tools"]);

    // Grants per workflow (read-only tools) and per task (wins).
    let yaml = "name: gatekept\nmcp: [{ server: docs }]\nsteps:\n  - id: approve\n    type: approval\n    who: [role:admin]\n";
    let (_, workflow) = call(
        app,
        "POST",
        "/api/v1/workflows",
        Some(json!({ "source_yaml": yaml })),
    )
    .await;
    let workflow_id = workflow["id"].clone();
    let (status, task) = call(
        app,
        "POST",
        "/api/v1/tasks",
        Some(json!({"title": "Use docs", "body": "", "workflow_id": workflow_id})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{task}");
    let path = format!("/api/v1/tasks/{}/mcp", task["id"].as_str().unwrap());
    let (_, grants) = call(app, "GET", &path, None).await;
    assert_eq!(grants, json!([]));
    let grant = json!([{"server": "docs", "tools": ["write_note"]}]);
    let (status, saved) = call(app, "PUT", &path, Some(grant.clone())).await;
    assert_eq!(status, StatusCode::OK, "{saved}");
    let (_, grants) = call(app, "GET", &path, None).await;
    assert_eq!(grants, grant);
    for bad in [
        json!([{"server": "nope"}]),
        json!([{"server": "docs"}, {"server": "docs"}]),
        json!([{"server": "docs", "extra": 1}]),
    ] {
        let (status, _) = call(app, "PUT", &path, Some(bad.clone())).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{bad}");
    }

    // The relay: a session token of the task's run reaches only its grant.
    let task_path = format!("/api/v1/tasks/{}", task["id"].as_str().unwrap());
    let mut run_id = Value::Null;
    for _ in 0..100 {
        run_id = call(app, "GET", &task_path, None).await.1["run"]["id"].clone();
        if run_id.is_string() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    let run_id = run_id.as_str().unwrap().parse().unwrap();
    let token = k
        .mcp
        .issue(run_id, sun_core::StepKey::new("approve").unwrap());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let kernel_url = format!("ws://{}", listener.local_addr().unwrap());
    let served = app.clone();
    tokio::spawn(async move { axum::serve(listener, served).await });
    let dial = |server: &str, token: &str| {
        let mut request = format!("{kernel_url}/mcp/{server}")
            .into_client_request()
            .unwrap();
        request.headers_mut().insert(
            header::AUTHORIZATION,
            format!("Bearer {token}").parse().unwrap(),
        );
        tokio_tungstenite::connect_async(request)
    };
    let status = |e: tokio_tungstenite::tungstenite::Error| match e {
        tokio_tungstenite::tungstenite::Error::Http(r) => r.status(),
        other => panic!("{other}"),
    };
    assert_eq!(
        status(dial("docs", "nope").await.unwrap_err()),
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        status(dial("other", &token).await.unwrap_err()),
        StatusCode::FORBIDDEN
    );
    let (mut socket, _) = dial("docs", &token).await.unwrap();
    let mut rpc = async |message: Value| {
        socket
            .send(Message::Text(message.to_string().into()))
            .await
            .unwrap();
        if message.get("id").is_none() {
            return Value::Null;
        }
        match socket.next().await.unwrap().unwrap() {
            Message::Text(text) => serde_json::from_str::<Value>(&text).unwrap(),
            other => panic!("{other:?}"),
        }
    };
    let initialized = rpc(
        json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {
        "protocolVersion": "2025-06-18", "capabilities": {},
        "clientInfo": {"name": "agent", "version": "0"}}}),
    )
    .await;
    assert_eq!(initialized["id"], 1, "{initialized}");
    rpc(json!({"jsonrpc": "2.0", "method": "notifications/initialized"})).await;
    let listed = rpc(json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"})).await;
    let names: Vec<&str> = listed["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    assert_eq!(
        names,
        ["write_note"],
        "the task's grant replaces the workflow's"
    );
    k.mcp.revoke(&token);
    assert_eq!(
        status(dial("docs", &token).await.unwrap_err()),
        StatusCode::UNAUTHORIZED
    );

    let (status, _) = call(app, "DELETE", "/api/v1/mcp_servers/docs", None).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (status, _) = call(app, "DELETE", "/api/v1/mcp_servers/docs", None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}
