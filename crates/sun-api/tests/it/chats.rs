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

/// BLO-162: a chat without an agent, straight with a tool. It takes exactly
/// one of an agent or a tool, switches model, effort or mode (a `settings`
/// entry, the title following the model) but never tool, credential or
/// host, and nothing while a reply is on its way. Only its creator reaches it.
#[tokio::test]
async fn a_direct_chat_talks_to_a_tool_and_switches_its_model_not_its_tool() {
    let k = kernel().await;
    let app = &k.app;
    let ana = member_token(&k.store, "ana", Role::Member).await;
    let bob = member_token(&k.store, "bob", Role::Member).await;

    // Exactly one of an agent or a tool, the tool with its workspace (asked
    // as the owner: a member naming no workspace is refused before this).
    let runtime = json!({ "tool": "claude-code", "model": "m1" });
    for wrong in [
        json!({}),
        json!({ "agent_id": "00000000000000000000000009", "workspace_id": MAIN, "runtime": runtime }),
        json!({ "runtime": runtime }),
        json!({ "workspace_id": MAIN }),
    ] {
        let (status, body) = call(app, "POST", "/api/v1/chats", Some(wrong)).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    }
    let start = json!({ "workspace_id": MAIN, "runtime": runtime, "permission_mode": "plan" });
    let (status, chat) = call_as(app, &ana, "POST", "/api/v1/chats", Some(start)).await;
    assert_eq!(status, StatusCode::CREATED, "{chat}");
    assert_eq!(
        (&chat["agent_id"], &chat["title"], &chat["runtime"]),
        (&json!(null), &json!("claude-code · m1"), &runtime)
    );
    assert_eq!(chat["permission_mode"], "plan");
    let id = chat["id"].as_str().unwrap();
    let patch = format!("/api/v1/chats/{id}");

    // Another model: a new title and a `settings` entry, no comment.
    let switch = json!({ "runtime": { "tool": "claude-code", "model": "m2" } });
    let (status, _) = call_as(app, &bob, "PATCH", &patch, Some(switch.clone())).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "bob reached it");
    let (status, chat) = call_as(app, &ana, "PATCH", &patch, Some(switch)).await;
    assert_eq!(status, StatusCode::OK, "{chat}");
    assert_eq!(
        (
            &chat["title"],
            &chat["runtime"]["model"],
            &chat["permission_mode"]
        ),
        (&json!("claude-code · m2"), &json!("m2"), &json!("plan"))
    );
    let activity = format!("/api/v1/tasks/{id}/activity");
    let (_, log) = call_as(app, &ana, "GET", &activity, None).await;
    let settings: Vec<&serde_json::Value> = log
        .as_array()
        .unwrap()
        .iter()
        .filter(|a| a["kind"] == "settings")
        .collect();
    assert_eq!(settings.len(), 1, "{log}");
    assert_eq!(settings[0]["body"]["from"]["runtime"]["model"], "m1");
    assert_eq!(settings[0]["body"]["to"]["runtime"]["model"], "m2");
    assert!(chat["last"].is_null(), "a switch is no message: {chat}");

    // One tool, credential and host per thread.
    for other in [
        json!({ "tool": "codex" }),
        json!({ "tool": "claude-code", "credential": "team-key" }),
        json!({ "tool": "claude-code", "host": "00000000000000000000000009" }),
    ] {
        let change = json!({ "runtime": other });
        let (status, body) = call_as(app, &ana, "PATCH", &patch, Some(change)).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    }
    // A chat with an agent has no settings to switch.
    let new_agent = json!({ "name": "Ada", "runtime_chain": ["claude-code"] });
    let (_, ada) = call(app, "POST", "/api/v1/agents", Some(new_agent)).await;
    let with_ada = json!({ "agent_id": ada["id"] });
    let (_, agent_chat) = call_as(app, &ana, "POST", "/api/v1/chats", Some(with_ada)).await;
    assert_eq!(agent_chat["runtime"], json!(null));
    let agent_patch = format!("/api/v1/chats/{}", agent_chat["id"].as_str().unwrap());
    let mode = json!({ "permission_mode": "plan" });
    let (status, _) = call_as(app, &ana, "PATCH", &agent_patch, Some(mode)).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);

    // A message queues a run (no host is connected): no switch until it ends.
    let say = json!({ "body": "Hello" });
    let comments = format!("/api/v1/tasks/{id}/comments");
    let (status, _) = call_as(app, &ana, "POST", &comments, Some(say)).await;
    assert_eq!(status, StatusCode::CREATED);
    let mode = json!({ "permission_mode": null });
    let (status, body) = call_as(app, &ana, "PATCH", &patch, Some(mode)).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert!(body.to_string().contains("Wait for the reply"), "{body}");

    // Listed for her only, with what it talks to.
    let list = format!("/api/v1/chats?workspace={MAIN}");
    let (_, mine) = call_as(app, &ana, "GET", &list, None).await;
    let direct: Vec<&serde_json::Value> = mine
        .as_array()
        .unwrap()
        .iter()
        .filter(|c| c["id"] == id)
        .collect();
    assert_eq!(direct[0]["runtime"]["model"], "m2", "{mine}");
    let (_, theirs) = call_as(app, &bob, "GET", &list, None).await;
    assert_eq!(theirs, json!([]));
}

/// BLO-162 §4: `bypass_permissions` on a chat without an agent needs what an
/// agent in it needs: `manage_runtimes` on the workspace (else 403, on
/// create and on switch) and the operator's `runtimes.allow_bypass` (else
/// 422, the owner included).
#[tokio::test]
async fn a_direct_chat_in_bypass_needs_manage_runtimes_and_allow_bypass() {
    let k = kernel().await;
    let bypass_ok = sun_api::router(k.api.clone().with_allow_bypass(true));
    let ana = member_token(&k.store, "ana", Role::Member).await;
    let alice = member_token(&k.store, "alice", Role::Admin).await;
    let start = |mode: &str| {
        Some(json!({
            "workspace_id": MAIN,
            "runtime": { "tool": "claude-code" },
            "permission_mode": mode,
        }))
    };

    let (status, body) = call_as(
        &bypass_ok,
        &ana,
        "POST",
        "/api/v1/chats",
        start("bypass_permissions"),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "a run-only member: {body}");
    let (status, chat) = call_as(&bypass_ok, &ana, "POST", "/api/v1/chats", start("plan")).await;
    assert_eq!(status, StatusCode::CREATED, "{chat}");
    let patch = format!("/api/v1/chats/{}", chat["id"].as_str().unwrap());
    let bypass = json!({ "permission_mode": "bypass_permissions" });
    let (status, body) = call_as(&bypass_ok, &ana, "PATCH", &patch, Some(bypass)).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "switching into it: {body}");

    let (status, body) = call_as(
        &k.app,
        &alice,
        "POST",
        "/api/v1/chats",
        start("bypass_permissions"),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "allow_bypass off: {body}"
    );
    let (status, body) = call(&k.app, "POST", "/api/v1/chats", start("bypass_permissions")).await;
    assert_eq!(
        status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "the owner: {body}"
    );
    let (status, chat) = call_as(
        &bypass_ok,
        &alice,
        "POST",
        "/api/v1/chats",
        start("bypass_permissions"),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{chat}");
    assert_eq!(chat["permission_mode"], "bypass_permissions");
}
