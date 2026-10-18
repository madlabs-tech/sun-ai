//! BLO-172: a capped person never acts on a live bypass session. Their steer
//! (`POST /runs/{id}/messages`) is queued for the next session and the
//! response says so; their answer to its `sun.ask`
//! (`POST /runs/{id}/questions/{qid}/answer`) is a `409` naming who can
//! answer. The engine decides (`sun_engine::bypass`); these check that each
//! route passes its caller through and maps the refusal.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use axum::http::{StatusCode, header};
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use sun_core::model::ActivityKind;
use sun_core::{StepKey, TaskId};
use sun_store::{EventLog, NewEvent, TaskRepo};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;

use crate::bypass_acts_for::{Live, live, with_host};
use crate::workspaces::{call, call_as};

const QUEUED: &str = "Queued for the next step: this session runs in bypass and you are capped.";

/// [`live`], its session journalled as running in bypass, as the engine
/// does for a bypass step. The policy is `off`, so member `bob` is capped.
async fn live_bypass() -> Live {
    let l = live().await;
    let mut tx = l.k.store.begin().await.unwrap();
    let events = tx.events_after(Some(l.run), 0, 10_000).await.unwrap();
    let assigned = events
        .iter()
        .find(|e| e.kind == "session_assigned")
        .expect("a session");
    let mut payload = assigned.payload.clone();
    payload["permission_mode"] = json!("bypass_permissions");
    let event = NewEvent::new(
        l.run,
        assigned.step_key.clone(),
        "session_assigned",
        payload,
    );
    tx.append_event(event).await.unwrap();
    tx.commit().await.unwrap();
    l
}

#[tokio::test]
async fn a_capped_steer_is_queued_and_says_so() {
    let mut l = live_bypass().await;
    let (app, bob) = (l.k.app.clone(), l.bob.clone());
    let path = format!("/api/v1/runs/{}/messages", l.run);
    let steer = Some(json!({ "body": "use tabs" }));
    let (status, event) = call_as(&app, &bob, "POST", &path, steer.clone()).await;
    assert_eq!(status, StatusCode::OK, "{event}");
    assert_eq!(event["kind"], "message_queued");
    assert_eq!(event["payload"]["summary"], QUEUED);
    assert_eq!(event["payload"]["acts_for"], l.bob_id);
    let task: TaskId = l.task.parse().unwrap();
    let thread =
        l.k.store
            .read()
            .await
            .unwrap()
            .activity_of(task, 0)
            .await
            .unwrap();
    assert!(
        thread
            .iter()
            .any(|a| a.kind == ActivityKind::Comment && a.body["text"] == QUEUED),
        "{thread:?}"
    );

    // The owner isn't capped: their steer reaches the session live.
    let (status, event) = with_host(&mut l.host, call(&app, "POST", &path, steer)).await;
    assert_eq!(status, StatusCode::OK, "{event}");
    assert_eq!(event["kind"], "intervention");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_capped_answer_is_a_409_and_the_owners_goes_through() {
    let mut l = live_bypass().await;
    let app = l.k.app.clone();
    // The agent asks over the `sun` MCP server, as a real session does.
    let token = l.k.mcp.issue(l.run, StepKey::new("work").unwrap());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let mut request = format!("ws://{}/mcp", listener.local_addr().unwrap())
        .into_client_request()
        .unwrap();
    let served = app.clone();
    tokio::spawn(async move { axum::serve(listener, served).await });
    let bearer = format!("Bearer {token}").parse().unwrap();
    request.headers_mut().insert(header::AUTHORIZATION, bearer);
    let (mut socket, _) = tokio_tungstenite::connect_async(request).await.unwrap();
    let init = json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {
        "protocolVersion": "2025-06-18", "capabilities": {},
        "clientInfo": {"name": "agent", "version": "0"}}});
    let initialized = json!({"jsonrpc": "2.0", "method": "notifications/initialized"});
    let ask = json!({"jsonrpc": "2.0", "id": 2, "method": "tools/call",
        "params": {"name": "ask", "arguments": {"question": "Tabs?"}}});
    for message in [init, initialized, ask] {
        let text = Message::Text(message.to_string().into());
        socket.send(text).await.unwrap();
    }
    let question = 'asked: {
        for _ in 0..500 {
            let mut tx = l.k.store.read().await.unwrap();
            let events = tx.events_after(Some(l.run), 0, 10_000).await.unwrap();
            if let Some(q) = events.iter().find(|e| e.kind == "question") {
                break 'asked q.payload["question_id"].as_str().unwrap().to_owned();
            }
            drop(tx);
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        panic!("the agent never asked");
    };

    let path = format!("/api/v1/runs/{}/questions/{question}/answer", l.run);
    let yes = Some(json!({ "answer": "yes" }));
    let (status, body) = call_as(&app, &l.bob, "POST", &path, yes.clone()).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    let message = body["error"]["message"].as_str().unwrap();
    assert!(
        message.contains("an owner or a member on the bypass allowlist"),
        "{message}"
    );

    let answer = with_host(&mut l.host, call(&app, "POST", &path, yes)).await;
    assert!(answer.0.is_success(), "{} {}", answer.0, answer.1);
    let reply = loop {
        match socket.next().await.unwrap().unwrap() {
            Message::Text(text) => {
                let reply: Value = serde_json::from_str(&text).unwrap();
                if reply["id"] == 2 {
                    break reply;
                }
            }
            _ => continue,
        }
    };
    assert_eq!(reply["result"]["content"][0]["text"], "yes", "{reply}");
}
