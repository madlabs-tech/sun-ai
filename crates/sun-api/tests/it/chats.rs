//! Chats over HTTP: a member's own conversations with agents. Their task,
//! activity, comment and run routes answer 404 to everyone but their
//! creator (another member, a deployment owner, the token holder), they
//! stay off task lists, take no key and take no mentions.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use axum::http::StatusCode;
use serde_json::json;
use sun_core::model::Role;

use crate::common::kernel;
use crate::workspaces::{call, call_as, member_token};

const MAIN: &str = "00000000000000000000000001";

#[tokio::test]
async fn a_chat_is_its_creators_alone() {
    let k = kernel().await;
    let app = &k.app;
    let new_agent = json!({ "name": "Ada", "runtime_chain": ["claude-code"] });
    let (_, ada) = call(app, "POST", "/api/v1/agents", Some(new_agent)).await;
    let ana = member_token(&k.store, "ana", Role::Member).await;
    let bob = member_token(&k.store, "bob", Role::Member).await;

    let start = json!({ "agent_id": ada["id"] });
    let (status, chat) = call_as(app, &ana, "POST", "/api/v1/chats", Some(start)).await;
    assert_eq!(status, StatusCode::CREATED, "{chat}");
    assert_eq!(
        (&chat["agent_id"], &chat["status"], &chat["title"]),
        (&ada["id"], &json!("needs_you"), &json!("Chat with Ada"))
    );
    let id = chat["id"].as_str().unwrap();
    let path = format!("/api/v1/tasks/{id}");
    let say = json!({ "body": "What does MAIN-1 need?" });
    let (status, _) = call_as(app, &ana, "POST", &format!("{path}/comments"), Some(say)).await;
    assert_eq!(status, StatusCode::CREATED);
    let mention = json!({ "body": "@Ada", "mentions": [ada["id"]] });
    let (status, _) = call_as(
        app,
        &ana,
        "POST",
        &format!("{path}/comments"),
        Some(mention),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "no mentions in a chat"
    );

    // Hers: listed with its last message and run, no key.
    let list = format!("/api/v1/chats?workspace={MAIN}");
    let (_, mine) = call_as(app, &ana, "GET", &list, None).await;
    assert_eq!(mine.as_array().unwrap().len(), 1, "{mine}");
    assert_eq!(mine[0]["last"]["body"]["text"], "What does MAIN-1 need?");
    let (status, view) = call_as(app, &ana, "GET", &path, None).await;
    assert_eq!((status, &view["key"]), (StatusCode::OK, &json!(null)));
    let run = mine[0]["run"]["id"].as_str().unwrap();
    let (status, _) = call_as(app, &ana, "GET", &format!("/api/v1/runs/{run}"), None).await;
    assert_eq!(status, StatusCode::OK);
    // A chat is no task.
    let (_, tasks) = call_as(
        app,
        &ana,
        "GET",
        &format!("/api/v1/tasks?workspace={MAIN}"),
        None,
    )
    .await;
    assert_eq!(tasks, json!([]));

    // Nobody else sees it: another member, nor the token holder.
    let (_, theirs) = call_as(app, &bob, "GET", &list, None).await;
    assert_eq!(theirs, json!([]));
    let reads = [
        path.clone(),
        format!("{path}/activity"),
        format!("{path}/runs"),
        format!("/api/v1/runs/{run}"),
        format!("/api/v1/runs/{run}/events"),
        format!("/api/v1/usage?run_id={run}"),
    ];
    for read in &reads {
        let (status, _) = call_as(app, &bob, "GET", read, None).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "bob: {read}");
        let (status, _) = call(app, "GET", read, None).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "token holder: {read}");
    }
    // Nor by naming it in a body.
    let eval = json!({ "name": "x", "pick": { "by": "tasks", "task_ids": [id] } });
    let (status, _) = call_as(app, &bob, "POST", "/api/v1/evals", Some(eval.clone())).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "bob: eval");
    let (status, _) = call(app, "POST", "/api/v1/evals", Some(eval)).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "token holder: eval");
    let say = json!({ "body": "sneaking in" });
    let (status, _) = call_as(app, &bob, "POST", &format!("{path}/comments"), Some(say)).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _) = call_as(app, &bob, "POST", &format!("{path}/interrupt"), None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // An archived agent takes no new chat; an unknown one is not found.
    let (status, _) = call(
        app,
        "DELETE",
        &format!("/api/v1/agents/{}", ada["id"].as_str().unwrap()),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let start = json!({ "agent_id": ada["id"] });
    let (status, _) = call_as(app, &ana, "POST", "/api/v1/chats", Some(start)).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    let start = json!({ "agent_id": "00000000000000000000000009" });
    let (status, _) = call_as(app, &ana, "POST", "/api/v1/chats", Some(start)).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}
