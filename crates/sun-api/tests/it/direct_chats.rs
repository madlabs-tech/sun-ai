//! BLO-166 (BLO-162 PR 2): trying to break `POST /chats` and
//! `PATCH /chats/{id}` for a chat without an agent. A body takes one shape
//! or is refused whole, a workspace the caller isn't in stays shut, the
//! bypass gate holds on both routes and under every spelling, a switch
//! waits for the reply, and nobody but the creator reaches the chat, or
//! learns from the answer that it exists.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use axum::Router;
use axum::http::StatusCode;
use serde_json::{Value, json};
use sun_core::model::Role;

use crate::common::{TOKEN, kernel};
use crate::workspaces::{call, call_as, member_token, member_token_in};

const MAIN: &str = "00000000000000000000000001";

/// Workspace `name`, made by the token holder: its id.
async fn workspace(app: &Router, name: &str) -> String {
    let body = json!({ "name": name });
    let (status, made) = call(app, "POST", "/api/v1/workspaces", Some(body)).await;
    assert_eq!(status, StatusCode::CREATED, "{made}");
    made["id"].as_str().unwrap().to_owned()
}

fn direct(workspace: &str, mode: Option<&str>) -> Value {
    json!({
        "workspace_id": workspace,
        "runtime": { "tool": "claude-code", "model": "m1" },
        "permission_mode": mode,
    })
}

/// `token`'s chats on `workspace`, as ids.
async fn chat_ids(app: &Router, token: &str, workspace: &str) -> Vec<String> {
    let list = format!("/api/v1/chats?workspace={workspace}");
    let (status, chats) = call_as(app, token, "GET", &list, None).await;
    assert_eq!(status, StatusCode::OK, "{chats}");
    chats
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["id"].as_str().unwrap().to_owned())
        .collect()
}

/// A body names an agent or a tool, never a mix and never half of one:
/// each wrong shape is 422 and leaves no chat behind.
#[tokio::test]
async fn post_chats_refuses_every_mixed_or_half_body_and_creates_nothing() {
    let k = kernel().await;
    let app = &k.app;
    let ana = member_token(&k.store, "ana", Role::Member).await;
    let new_agent = json!({ "name": "Ada", "runtime_chain": ["claude-code"] });
    let (_, ada) = call(app, "POST", "/api/v1/agents", Some(new_agent)).await;
    let agent = &ada["id"];
    let runtime = json!({ "tool": "claude-code", "model": "m1" });

    let wrong = [
        (
            "agent and mode",
            json!({ "agent_id": agent, "permission_mode": "plan" }),
        ),
        (
            "agent and runtime",
            json!({ "agent_id": agent, "runtime": runtime }),
        ),
        (
            "agent and workspace",
            json!({ "agent_id": agent, "workspace_id": MAIN }),
        ),
        (
            "all three",
            json!({ "agent_id": agent, "workspace_id": MAIN, "runtime": runtime }),
        ),
        (
            "workspace and mode",
            json!({ "workspace_id": MAIN, "permission_mode": "plan" }),
        ),
        (
            "runtime is null",
            json!({ "workspace_id": MAIN, "runtime": null }),
        ),
        (
            "unknown field",
            json!({ "workspace_id": MAIN, "runtime": runtime, "instructions": "be evil" }),
        ),
        (
            "unknown runtime field",
            json!({ "workspace_id": MAIN, "runtime": { "tool": "claude-code", "skills": [] } }),
        ),
        (
            "runtime without a tool",
            json!({ "workspace_id": MAIN, "runtime": { "model": "m1" } }),
        ),
        (
            "unknown credential",
            json!({ "workspace_id": MAIN, "runtime": { "tool": "claude-code", "credential": "nope" } }),
        ),
        (
            "unknown mode",
            json!({ "workspace_id": MAIN, "runtime": runtime, "permission_mode": "root" }),
        ),
        (
            "runtime as an array",
            json!({ "workspace_id": MAIN, "runtime": [runtime] }),
        ),
    ];
    for (case, body) in wrong {
        let (status, answer) = call_as(app, &ana, "POST", "/api/v1/chats", Some(body)).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{case}: {answer}");
    }
    // A workspace id the guard can't read names no workspace: refused there.
    let body = json!({ "workspace_id": 1, "runtime": runtime });
    let (status, answer) = call_as(app, &ana, "POST", "/api/v1/chats", Some(body)).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{answer}");
    assert_eq!(chat_ids(app, &ana, MAIN).await, Vec::<String>::new());

    // A tool name is checked like an agent's chain entry, which takes any
    // name (`ChainSpec::tool` is free text): one Sun has no adapter for is
    // accepted and titled with it. Reported on BLO-166 as a gap shared with
    // agents, not a PR 2 regression.
    let body = json!({ "workspace_id": MAIN, "runtime": { "tool": "rm-rf" } });
    let (status, chat) = call_as(app, &ana, "POST", "/api/v1/chats", Some(body)).await;
    assert_eq!(
        (status, &chat["title"]),
        (StatusCode::CREATED, &json!("rm-rf")),
        "{chat}"
    );

    // An explicit null agent is no agent: the direct shape still holds.
    let body = json!({ "agent_id": null, "workspace_id": MAIN, "runtime": runtime });
    let (status, chat) = call_as(app, &ana, "POST", "/api/v1/chats", Some(body)).await;
    assert_eq!(status, StatusCode::CREATED, "{chat}");
    assert_eq!(chat["agent_id"], json!(null));
}

/// A direct chat goes on a workspace the caller is a running member of. To
/// one who isn't on it at all (no workspace, or another one) it is hidden
/// like a missing workspace (404, as everywhere else in tenancy); a viewer
/// on it lacks `run` (403). None of them leaves a chat.
#[tokio::test]
async fn post_chats_needs_run_on_the_named_workspace() {
    let k = kernel().await;
    let app = &k.app;
    let ops = workspace(app, "Ops").await;
    let nobody = member_token_in(&k.store, "nobody", &[]).await;
    let ops_only = member_token_in(&k.store, "olga", &[(ops.parse().unwrap(), Role::Member)]).await;
    let viewer = member_token(&k.store, "vic", Role::Viewer).await;

    for (who, token, want) in [
        ("no workspace", &nobody, StatusCode::NOT_FOUND),
        ("Ops only", &ops_only, StatusCode::NOT_FOUND),
        ("viewer", &viewer, StatusCode::FORBIDDEN),
    ] {
        let (status, body) = call_as(
            app,
            token,
            "POST",
            "/api/v1/chats",
            Some(direct(MAIN, None)),
        )
        .await;
        assert_eq!(status, want, "{who} on Main: {body}");
    }
    // Naming no workspace is refused before the shape is read.
    let half = json!({ "runtime": { "tool": "claude-code" } });
    let (status, body) = call_as(app, &nobody, "POST", "/api/v1/chats", Some(half)).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "no workspace named: {body}");
    // A workspace that doesn't exist.
    let ghost = sun_core::WorkspaceId::generate().to_string();
    let (status, body) = call_as(
        app,
        &ops_only,
        "POST",
        "/api/v1/chats",
        Some(direct(&ghost, None)),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "unknown workspace: {body}");

    let (_, main_chats) = call(app, "GET", &format!("/api/v1/chats?workspace={MAIN}"), None).await;
    assert_eq!(main_chats, json!([]), "nothing made on Main");
    // Her own workspace works.
    let (status, chat) = call_as(
        app,
        &ops_only,
        "POST",
        "/api/v1/chats",
        Some(direct(&ops, None)),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{chat}");
    assert_eq!(chat["workspace_id"], json!(ops));
}

/// The bypass gate on both routes: an admin of one workspace is a member
/// of another and can't bypass there; with `allow_bypass` off a switch into
/// it is 422 even for an admin; no other spelling of it is ever accepted,
/// on create or on switch.
#[tokio::test]
async fn bypass_needs_manage_runtimes_on_that_workspace_and_no_spelling_slips_by() {
    let k = kernel().await;
    let bypass_ok = sun_api::router(k.api.clone().with_allow_bypass(true));
    let ops = workspace(&k.app, "Ops").await;
    let alice = member_token_in(
        &k.store,
        "alice",
        &[
            (sun_core::WorkspaceId::MAIN, Role::Member),
            (ops.parse().unwrap(), Role::Admin),
        ],
    )
    .await;
    let bypass = Some("bypass_permissions");

    // Admin on Ops, member on Main: bypass only on Ops.
    let (status, body) = call_as(
        &bypass_ok,
        &alice,
        "POST",
        "/api/v1/chats",
        Some(direct(MAIN, bypass)),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "Main: {body}");
    let (status, on_ops) = call_as(
        &bypass_ok,
        &alice,
        "POST",
        "/api/v1/chats",
        Some(direct(&ops, bypass)),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "Ops: {on_ops}");

    // Switching into it on Main is refused too, and changes nothing.
    let (_, on_main) = call_as(
        &bypass_ok,
        &alice,
        "POST",
        "/api/v1/chats",
        Some(direct(MAIN, None)),
    )
    .await;
    let main_patch = format!("/api/v1/chats/{}", on_main["id"].as_str().unwrap());
    let into = json!({ "permission_mode": "bypass_permissions" });
    let (status, body) =
        call_as(&bypass_ok, &alice, "PATCH", &main_patch, Some(into.clone())).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "switch on Main: {body}");

    // On Ops with allow_bypass off (the default router): 422, the mode kept.
    let (_, plain) = call_as(
        &k.app,
        &alice,
        "POST",
        "/api/v1/chats",
        Some(direct(&ops, None)),
    )
    .await;
    let ops_patch = format!("/api/v1/chats/{}", plain["id"].as_str().unwrap());
    let (status, body) = call_as(&k.app, &alice, "PATCH", &ops_patch, Some(into.clone())).await;
    assert_eq!(
        status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "allow_bypass off: {body}"
    );
    // ...and allowed once the operator allows it.
    let (status, switched) = call_as(&bypass_ok, &alice, "PATCH", &ops_patch, Some(into)).await;
    assert_eq!(status, StatusCode::OK, "{switched}");
    assert_eq!(switched["permission_mode"], "bypass_permissions");

    // No other spelling gets in, for the admin with bypass allowed.
    let spellings = [
        "bypassPermissions",
        "BypassPermissions",
        "BYPASS_PERMISSIONS",
        "bypass-permissions",
        "bypass permissions",
        " bypass_permissions",
        "bypass_permissions\n",
        "dangerously-skip-permissions",
        "dangerouslySkipPermissions",
        "yolo",
        "",
    ];
    for spelling in spellings {
        let (status, body) = call_as(
            &bypass_ok,
            &alice,
            "POST",
            "/api/v1/chats",
            Some(direct(&ops, Some(spelling))),
        )
        .await;
        assert_eq!(
            status,
            StatusCode::UNPROCESSABLE_ENTITY,
            "create {spelling:?}: {body}"
        );
        let change = json!({ "permission_mode": spelling });
        let (status, body) = call_as(&bypass_ok, &alice, "PATCH", &main_patch, Some(change)).await;
        assert_eq!(
            status,
            StatusCode::UNPROCESSABLE_ENTITY,
            "switch {spelling:?}: {body}"
        );
    }
    let (_, main_chat) = call_as(
        &bypass_ok,
        &alice,
        "GET",
        &format!("/api/v1/tasks/{}", on_main["id"].as_str().unwrap()),
        None,
    )
    .await;
    assert_eq!(
        main_chat["title"], "claude-code · m1",
        "untouched: {main_chat}"
    );
    let listed = chat_ids(&bypass_ok, &alice, &ops).await;
    assert_eq!(listed.len(), 2, "only the two real Ops chats: {listed:?}");
}

/// While a reply is on its way (queued, or placed and running) every real
/// switch is 422 and logs nothing; a switch to what it already is answers
/// the chat as it is.
#[tokio::test]
async fn patch_while_queued_is_refused_and_changes_nothing() {
    let k = kernel().await;
    let app = &k.app;
    let ana = member_token(&k.store, "ana", Role::Member).await;
    let (_, chat) = call_as(
        app,
        &ana,
        "POST",
        "/api/v1/chats",
        Some(direct(MAIN, Some("plan"))),
    )
    .await;
    let id = chat["id"].as_str().unwrap();
    let patch = format!("/api/v1/chats/{id}");
    let say = json!({ "body": "Hello" });
    let (status, _) = call_as(
        app,
        &ana,
        "POST",
        &format!("/api/v1/tasks/{id}/comments"),
        Some(say),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let (_, runs) = call_as(app, &ana, "GET", &format!("/api/v1/tasks/{id}/runs"), None).await;
    assert_eq!(runs[0]["status"], "queued", "{runs}");
    let activity = format!("/api/v1/tasks/{id}/activity");
    let settings_logged = || async {
        let (_, log) = call_as(app, &ana, "GET", &activity, None).await;
        log.as_array()
            .unwrap()
            .iter()
            .filter(|a| a["kind"] == "settings")
            .count()
    };

    for change in [
        json!({ "runtime": { "tool": "claude-code", "model": "m2" } }),
        json!({ "runtime": { "tool": "claude-code", "model": "m1", "effort": "high" } }),
        json!({ "permission_mode": "default" }),
        json!({ "permission_mode": null }),
    ] {
        let (status, body) = call_as(app, &ana, "PATCH", &patch, Some(change.clone())).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{change}: {body}");
    }
    assert_eq!(settings_logged().await, 0, "a refused switch logs nothing");
    let (_, now) = call_as(
        app,
        &ana,
        "GET",
        &format!("/api/v1/chats?workspace={MAIN}"),
        None,
    )
    .await;
    assert_eq!(
        (
            &now[0]["runtime"]["model"],
            &now[0]["permission_mode"],
            &now[0]["title"]
        ),
        (&json!("m1"), &json!("plan"), &json!("claude-code · m1")),
        "{now}"
    );

    // The same settings again is no switch: the chat as it is.
    let same =
        json!({ "runtime": { "tool": "claude-code", "model": "m1" }, "permission_mode": "plan" });
    let (status, body) = call_as(app, &ana, "PATCH", &patch, Some(same)).await;
    assert_eq!(status, StatusCode::OK, "{body}");
}

/// The refusal says "Wait for the reply or stop it, then switch": after a
/// person stops the reply, the switch goes through. Stop pauses the run
/// (`Paused`, shown `waiting_approval`) rather than ending it, and a paused
/// run still counts as on its way, so this is red until the switch treats a
/// run a person stopped as over (BLO-162 bug filed from BLO-166).
#[tokio::test]
async fn a_direct_chat_switches_after_its_reply_is_stopped() {
    let k = kernel().await;
    let app = &k.app;
    let ana = member_token(&k.store, "ana", Role::Member).await;
    let (_, chat) = call_as(app, &ana, "POST", "/api/v1/chats", Some(direct(MAIN, None))).await;
    let id = chat["id"].as_str().unwrap();
    let say = json!({ "body": "Hello" });
    let comments = format!("/api/v1/tasks/{id}/comments");
    let (status, _) = call_as(app, &ana, "POST", &comments, Some(say)).await;
    assert_eq!(status, StatusCode::CREATED);
    // Stop needs the run going; it is placed on the kernel's host.
    let runs = format!("/api/v1/tasks/{id}/runs");
    let run_status = || async {
        let (_, list) = call_as(app, &ana, "GET", &runs, None).await;
        list.as_array().unwrap().last().unwrap()["status"]
            .as_str()
            .unwrap()
            .to_owned()
    };
    let mut status_now = run_status().await;
    for _ in 0..300 {
        if status_now == "running" {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        status_now = run_status().await;
    }
    assert_eq!(status_now, "running");
    let stop = format!("/api/v1/tasks/{id}/interrupt");
    let (status, body) = call_as(app, &ana, "POST", &stop, None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let mut stopped = run_status().await;
    for _ in 0..300 {
        if stopped != "running" {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        stopped = run_status().await;
    }

    let switch = json!({ "runtime": { "tool": "claude-code", "model": "m2" } });
    let (status, body) = call_as(
        app,
        &ana,
        "PATCH",
        &format!("/api/v1/chats/{id}"),
        Some(switch),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "run {stopped:?} after Stop: {body}");
    assert_eq!(body["runtime"]["model"], "m2");
}

/// `PATCH /chats/{id}` answers 404 to everyone but the creator (another
/// member, an admin, a member of another workspace, the token holder),
/// whatever the body, so the answer never tells that the chat exists. A
/// task that isn't a direct chat, a missing id and a malformed one are
/// refused without a change.
#[tokio::test]
async fn only_the_creator_reaches_a_direct_chat_whatever_the_body() {
    let k = kernel().await;
    let bypass_ok = sun_api::router(k.api.clone().with_allow_bypass(true));
    let app = &bypass_ok;
    let ops = workspace(app, "Ops").await;
    let ana = member_token(&k.store, "ana", Role::Member).await;
    let bob = member_token(&k.store, "bob", Role::Member).await;
    let alice = member_token(&k.store, "alice", Role::Admin).await;
    let olga = member_token_in(&k.store, "olga", &[(ops.parse().unwrap(), Role::Owner)]).await;
    let (_, chat) = call_as(app, &ana, "POST", "/api/v1/chats", Some(direct(MAIN, None))).await;
    let id = chat["id"].as_str().unwrap();
    let patch = format!("/api/v1/chats/{id}");

    let bodies = [
        json!({ "runtime": { "tool": "claude-code", "model": "m2" } }),
        json!({ "permission_mode": "bypass_permissions" }),
        json!({ "runtime": { "tool": "codex" } }),
        json!({ "not_a_field": true }),
        json!({}),
    ];
    for (who, token) in [
        ("bob", &bob),
        ("alice (admin)", &alice),
        ("olga (Ops)", &olga),
        ("token holder", &TOKEN.to_owned()),
    ] {
        for body in &bodies {
            let (status, answer) = call_as(app, token, "PATCH", &patch, Some(body.clone())).await;
            assert_eq!(status, StatusCode::NOT_FOUND, "{who} {body}: {answer}");
        }
        let (status, _) = call_as(
            app,
            token,
            "GET",
            &format!("/api/v1/tasks/{id}/activity"),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{who}: activity");
        let (status, _) = call_as(
            app,
            token,
            "POST",
            &format!("/api/v1/tasks/{id}/comments"),
            Some(json!({ "body": "hi" })),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{who}: comment");
    }
    let (_, mine) = call_as(
        app,
        &ana,
        "GET",
        &format!("/api/v1/chats?workspace={MAIN}"),
        None,
    )
    .await;
    assert_eq!(
        (
            &mine[0]["runtime"]["model"],
            &mine[0]["permission_mode"],
            &mine[0]["last"]
        ),
        (&json!("m1"), &json!(null), &json!(null)),
        "nobody changed or said anything: {mine}"
    );

    // Not a direct chat, not there, not an id.
    let (_, task) = call(
        app,
        "POST",
        "/api/v1/tasks",
        Some(json!({ "title": "t", "workspace_id": MAIN })),
    )
    .await;
    let switch = json!({ "runtime": { "tool": "claude-code", "model": "m2" } });
    let task_patch = format!("/api/v1/chats/{}", task["id"].as_str().unwrap());
    let (status, body) = call(app, "PATCH", &task_patch, Some(switch.clone())).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "a task: {body}");
    let ghost = format!("/api/v1/chats/{}", sun_core::TaskId::generate());
    let (status, body) = call(app, "PATCH", &ghost, Some(switch.clone())).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "missing: {body}");
    let (status, body) = call(app, "PATCH", "/api/v1/chats/not-an-id", Some(switch)).await;
    assert!(
        matches!(
            status,
            StatusCode::UNPROCESSABLE_ENTITY | StatusCode::NOT_FOUND
        ),
        "malformed: {status} {body}"
    );
    let (_, after) = call(
        app,
        "GET",
        &format!("/api/v1/tasks/{}", task["id"].as_str().unwrap()),
        None,
    )
    .await;
    assert_eq!(after["title"], "t", "{after}");
}
