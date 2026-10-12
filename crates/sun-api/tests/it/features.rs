//! Feature toggles, timeline search and usage per
//! runtime with the subscription quota window.
#![allow(clippy::unwrap_used)]

use crate::common;

use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use common::{TOKEN, kernel, kernel_with_features};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use sun_core::{RunId, SessionId};
use sun_store::{EventLog, NewEvent};
use tower::ServiceExt;

async fn call(app: &Router, method: &str, path: &str) -> (StatusCode, Value) {
    let request = Request::builder()
        .method(method)
        .uri(path)
        .header(header::AUTHORIZATION, format!("Bearer {TOKEN}"))
        .body(Body::empty())
        .unwrap();
    let response = app.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

#[tokio::test]
async fn knowledge_off_returns_404() {
    let off = sun_api::Features {
        knowledge: false,
        ..Default::default()
    };
    let k = kernel_with_features(off).await;
    for path in [
        "/api/v1/knowledge/sources",
        "/api/v1/knowledge/search?q=x",
        "/api/v1/settings/embeddings",
    ] {
        let (status, body) = call(&k.app, "GET", path).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{path}");
        assert!(
            body["error"]["message"]
                .as_str()
                .unwrap()
                .contains("features.knowledge"),
            "{body}"
        );
    }
    // The rest runs, and the UI learns what's off.
    assert_eq!(call(&k.app, "GET", "/api/v1/evals").await.0, StatusCode::OK);
    let (status, features) = call(&k.app, "GET", "/api/v1/features").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(features["knowledge"], false);
    assert_eq!(features["catalog"], true);
    // Still authenticated: no token, no answer.
    let anonymous = Request::get("/api/v1/features")
        .body(Body::empty())
        .unwrap();
    let response = k.app.clone().oneshot(anonymous).await.unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    // On by default.
    let k = kernel().await;
    let (status, _) = call(&k.app, "GET", "/api/v1/knowledge/sources").await;
    assert_eq!(status, StatusCode::OK);
}

const FLOW: &str = "name: f\nsteps:\n  - id: work\n    type: agent\n    prompt: hi\n";

/// A task's run, once the queue started it.
async fn started_run(app: &Router) -> RunId {
    let request = |path: &str, body: Value| {
        Request::post(path)
            .header(header::AUTHORIZATION, format!("Bearer {TOKEN}"))
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(body.to_string()))
            .unwrap()
    };
    let read = |response: axum::response::Response| async {
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        serde_json::from_slice::<Value>(&bytes).unwrap()
    };
    let workflow = request("/api/v1/workflows", json!({ "source_yaml": FLOW }));
    let workflow = read(app.clone().oneshot(workflow).await.unwrap()).await;
    let task = json!({ "title": "t", "workflow_id": workflow["id"] });
    let task = read(
        app.clone()
            .oneshot(request("/api/v1/tasks", task))
            .await
            .unwrap(),
    )
    .await;
    let path = format!("/api/v1/tasks/{}", task["id"].as_str().unwrap());
    for _ in 0..200 {
        let (_, view) = call(app, "GET", &path).await;
        if let Some(run) = view["run"]["id"].as_str() {
            return run.parse().unwrap();
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("the task never started");
}

#[tokio::test]
async fn timeline_filters_and_usage_per_runtime() {
    let k = kernel().await;
    let run = started_run(&k.app).await;
    let session = SessionId::generate();
    let mut tx = k.store.begin().await.unwrap();
    let assigned = json!({ "session_id": session, "adapter": "claude-code" });
    tx.append_event(NewEvent::new(run, None, "session_assigned", assigned))
        .await
        .unwrap();
    let host_events = [
        (
            "tool_call",
            json!({ "kind": "tool_call", "name": "Bash", "input": "cargo test" }),
        ),
        (
            "tool_call",
            json!({ "kind": "tool_call", "name": "Read", "input": "src/lib.rs" }),
        ),
        (
            "text",
            json!({ "kind": "text", "text": "Running CARGO test now" }),
        ),
        (
            "usage",
            json!({ "kind": "usage", "input": 100, "output": 20, "cost_usd": 0.5 }),
        ),
    ];
    for (seq, (kind, payload)) in (1..).zip(host_events) {
        let event = NewEvent::new(run, None, kind, payload);
        tx.append_host_event(session, seq, event).await.unwrap();
    }
    tx.commit().await.unwrap();

    let events = format!("/api/v1/runs/{run}/events");
    let (_, tools) = call(&k.app, "GET", &format!("{events}?kind=tool_call")).await;
    let kinds: Vec<&str> = tools
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["kind"].as_str().unwrap())
        .collect();
    assert_eq!(kinds, ["tool_call", "tool_call"]);
    // Text search is case-insensitive; `%` is a literal, not a wildcard.
    let (_, cargo) = call(&k.app, "GET", &format!("{events}?q=Cargo")).await;
    assert_eq!(cargo.as_array().unwrap().len(), 2, "{cargo}");
    let (_, both) = call(&k.app, "GET", &format!("{events}?kind=text&q=cargo")).await;
    assert_eq!(both[0]["payload"]["text"], "Running CARGO test now");
    let (_, none) = call(&k.app, "GET", &format!("{events}?q=%25")).await;
    assert_eq!(none, json!([]));
    let (status, _) = call(&k.app, "GET", &format!("{events}?kind=DROP%20TABLE")).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);

    let (status, usage) = call(&k.app, "GET", "/api/v1/usage?by=runtime").await;
    assert_eq!(status, StatusCode::OK, "{usage}");
    let tokens = json!({ "input_tokens": 100, "output_tokens": 20, "cost_usd": 0.5 });
    assert_eq!(
        usage,
        json!([{
            "runtime": "claude-code",
            "source": "byo_runtime",
            "usage": tokens,
            "quota_window": { "hours": 5, "usage": tokens },
        }])
    );
    let (status, _) = call(&k.app, "GET", "/api/v1/usage?by=host").await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
}
