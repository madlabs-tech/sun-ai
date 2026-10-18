//! BLO-164 / BLO-167: every HTTP route that queues a run records who made it
//! go (`by` on the run's first `run_status` event), because the engine caps a
//! bypass session by that person (`sun_engine::bypass`). A route that drops
//! the caller lets a member run a saved bypass workflow as the deployment.
//! What a client claims about itself changes nothing.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use axum::Router;
use axum::http::StatusCode;
use serde_json::{Value, json};
use sun_core::TaskId;
use sun_core::model::Role;
use sun_store::{EventLog, RunRepo};

use crate::common::{Kernel, kernel};
use crate::mode_unoffered::ended_runs;
use crate::workspaces::{call, call_as, member_token};

const MAIN: &str = "00000000000000000000000001";

/// A saved bypass workflow a webhook can start.
const HOOKED: &str = "name: hooked
on: [manual, webhook]
exec_env: { kind: container, image: 'node:22' }
steps:
  - {id: implement, type: agent, prompt: fix it, permission_mode: bypass_permissions}
";

/// `by` of the first `run_status` event of `task`'s latest run.
async fn by_of_latest(k: &Kernel, task: &Value) -> Value {
    let task: TaskId = task["id"].as_str().unwrap().parse().unwrap();
    let mut tx = k.store.read().await.unwrap();
    let runs = tx.runs_of_task(task).await.unwrap();
    let run = runs
        .last()
        .unwrap_or_else(|| panic!("task {task} never queued"));
    let first = tx.events_after(Some(run.id), 0, 1).await.unwrap();
    assert_eq!(first[0].kind, "run_status");
    first[0].payload.get("by").cloned().unwrap_or(Value::Null)
}

async fn runs_of(k: &Kernel, task: &Value) -> usize {
    let task: TaskId = task["id"].as_str().unwrap().parse().unwrap();
    let mut tx = k.store.read().await.unwrap();
    tx.runs_of_task(task).await.unwrap().len()
}

/// `(status, body)` of `method path` as the token holder, asserting `2xx`.
async fn ok(app: &Router, method: &str, path: &str, body: Option<Value>) -> Value {
    let (status, body) = call(app, method, path, body).await;
    assert!(status.is_success(), "{method} {path}: {status} {body}");
    body
}

async fn ok_as(app: &Router, token: &str, method: &str, path: &str, body: Option<Value>) -> Value {
    let (status, body) = call_as(app, token, method, path, body).await;
    assert!(status.is_success(), "{method} {path}: {status} {body}");
    body
}

struct Setup {
    k: Kernel,
    app: Router,
    bob: String,
    bob_id: Value,
    workflow: String,
    agent: String,
}

/// A kernel with sun.yaml `allow_bypass` on, the policy at its default
/// (`off`), member `bob`, the saved [`HOOKED`] workflow and an agent.
async fn setup() -> Setup {
    let k = kernel().await;
    let app = sun_api::router(k.api.clone().with_allow_bypass(true));
    let bob = member_token(&k.store, "bob", Role::Member).await;
    let me = ok_as(&app, &bob, "GET", "/api/v1/me", None).await;
    let bob_id = me["memberships"][0]["member_id"].clone();
    assert!(bob_id.is_string(), "{me}");
    assert_eq!(me["can_bypass"], false, "off by default");
    let save = json!({ "source_yaml": HOOKED, "workspace_id": MAIN });
    let workflow = ok(&app, "POST", "/api/v1/workflows", Some(save)).await;
    let new_agent = json!({ "name": "Ada", "runtime_chain": ["claude-code"] });
    let agent = ok(&app, "POST", "/api/v1/agents", Some(new_agent)).await;
    Setup {
        workflow: workflow["id"].as_str().unwrap().to_owned(),
        agent: agent["id"].as_str().unwrap().to_owned(),
        k,
        app,
        bob,
        bob_id,
    }
}

impl Setup {
    /// A task the token holder (an owner) creates.
    async fn owners_task(&self, body: Value) -> Value {
        let mut body = body;
        body["title"] = json!("Fix it");
        body["workspace_id"] = json!(MAIN);
        ok(&self.app, "POST", "/api/v1/tasks", Some(body)).await
    }

    fn path(task: &Value) -> String {
        format!("/api/v1/tasks/{}", task["id"].as_str().unwrap())
    }
}

/// Case 5: creating, moving, commenting on, @mentioning, assigning,
/// retrying and chatting each queue as the member who did it.
#[tokio::test]
async fn every_board_route_queues_as_the_calling_member() {
    let s = setup().await;
    let (app, bob, bob_id) = (&s.app, s.bob.as_str(), &s.bob_id);

    // Created by the member.
    let create = json!({ "title": "Fix it", "workflow_id": s.workflow });
    let task = ok_as(app, bob, "POST", "/api/v1/tasks", Some(create)).await;
    assert_eq!(&by_of_latest(&s.k, &task).await, bob_id, "created");

    // The owner's backlog task, moved to Todo by the member.
    let task = s
        .owners_task(json!({ "workflow_id": s.workflow, "status": "backlog" }))
        .await;
    assert_eq!(runs_of(&s.k, &task).await, 0);
    let todo = Some(json!({ "status": "todo" }));
    ok_as(app, bob, "PATCH", &Setup::path(&task), todo).await;
    assert_eq!(&by_of_latest(&s.k, &task).await, bob_id, "moved");

    // The owner's blocked agent task, woken by the member's comment.
    let assignee = json!({ "kind": "agent", "id": s.agent });
    let task = s
        .owners_task(json!({ "assignee": assignee, "status": "backlog" }))
        .await;
    let blocked = Some(json!({ "status": "blocked" }));
    ok(app, "PATCH", &Setup::path(&task), blocked).await;
    assert_eq!(runs_of(&s.k, &task).await, 0);
    let say = Some(json!({ "body": "go on" }));
    let comments = format!("{}/comments", Setup::path(&task));
    ok_as(app, bob, "POST", &comments, say).await;
    assert_eq!(&by_of_latest(&s.k, &task).await, bob_id, "comment");

    // The owner's plain task, assigned to the agent by the member.
    let task = s.owners_task(json!({})).await;
    let assign = Some(json!({ "assignee": assignee }));
    ok_as(app, bob, "PATCH", &Setup::path(&task), assign).await;
    assert_eq!(&by_of_latest(&s.k, &task).await, bob_id, "assigned");

    // The owner's plain task; the member @mentions the agent on it.
    let task = s.owners_task(json!({})).await;
    let comments = format!("{}/comments", Setup::path(&task));
    let mention = Some(json!({ "body": "@Ada look", "mentions": [s.agent] }));
    ok_as(app, bob, "POST", &comments, mention).await;
    let source: TaskId = task["id"].as_str().unwrap().parse().unwrap();
    let chat = {
        use sun_store::TaskRepo;
        let mut tx = s.k.store.read().await.unwrap();
        let agent = s.agent.parse().unwrap();
        tx.mention_chat(source, agent).await.unwrap().unwrap()
    };
    let chat = json!({ "id": chat.id.to_string() });
    assert_eq!(&by_of_latest(&s.k, &chat).await, bob_id, "mention");

    // The owner's run, once it has ended, retried by the member. Waits for
    // the end instead of cancelling: the queue may refuse (fail) the run
    // before a cancel lands, and Failed can't move to Cancelled (BLO-179).
    let task = s.owners_task(json!({ "workflow_id": s.workflow })).await;
    assert_eq!(by_of_latest(&s.k, &task).await, "token", "the token holder");
    let runs = ended_runs(&s.k, task["id"].as_str().unwrap()).await;
    assert_eq!(runs.len(), 1);
    let retry = format!("/api/v1/runs/{}/retry", runs[0].id);
    ok_as(app, bob, "POST", &retry, None).await;
    assert_eq!(runs_of(&s.k, &task).await, 2);
    assert_eq!(&by_of_latest(&s.k, &task).await, bob_id, "retry");

    // The member's chat with the agent, their first message.
    let start = Some(json!({ "agent_id": s.agent }));
    let chat = ok_as(app, bob, "POST", "/api/v1/chats", start).await;
    let say = Some(json!({ "body": "hi" }));
    let comments = format!("{}/comments", Setup::path(&chat));
    ok_as(app, bob, "POST", &comments, say).await;
    assert_eq!(&by_of_latest(&s.k, &chat).await, bob_id, "chat");
}

/// Case 5 (Rimuru's finding): a member who may not bypass calls a saved
/// bypass workflow's webhook. The run it queues must act for them, so the
/// engine caps it. RED until `POST /workflows/{id}/trigger` passes the
/// caller to `Engine::trigger_webhook` (today it queues with no `by`, i.e.
/// as the deployment, which always bypasses).
#[tokio::test]
async fn a_members_webhook_call_queues_as_them() {
    let s = setup().await;
    let trigger = format!("/api/v1/workflows/{}/trigger", s.workflow);
    let hook = Some(json!({ "title": "Deploy failed", "key": "build-77" }));
    let (status, task) = call_as(&s.app, &s.bob, "POST", &trigger, hook).await;
    assert_eq!(status, StatusCode::CREATED, "{task}");
    assert_eq!(
        by_of_latest(&s.k, &task).await,
        s.bob_id,
        "the member's webhook call queued a bypass run as the deployment"
    );
}

/// Case 7: only a deployment owner reads or sets the policy, an admin
/// included; a forged `can_bypass` is refused, not trusted, and `/me` keeps
/// answering from the server's policy.
#[tokio::test]
async fn a_forged_can_bypass_claim_changes_nothing() {
    let s = setup().await;
    let app = &s.app;
    let ann = member_token(&s.k.store, "ann", Role::Admin).await;
    for who in [ann.as_str(), s.bob.as_str()] {
        let (status, _) = call_as(app, who, "GET", "/api/v1/settings/bypass", None).await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        let everyone = Some(json!({ "mode": "everyone" }));
        let (status, _) = call_as(app, who, "PUT", "/api/v1/settings/bypass", everyone).await;
        assert_eq!(status, StatusCode::FORBIDDEN);
    }
    let policy = ok(app, "GET", "/api/v1/settings/bypass", None).await;
    assert_eq!(policy["mode"], "off", "unchanged by the refused PUTs");

    // The owner can't smuggle a client flag into the policy either.
    let forged = Some(json!({ "mode": "off", "can_bypass": true }));
    let (status, _) = call(app, "PUT", "/api/v1/settings/bypass", forged).await;
    assert!(status.is_client_error(), "{status}");

    // A member's task claiming `can_bypass` is refused, not run uncapped.
    let create = json!({ "title": "Fix it", "workflow_id": s.workflow, "can_bypass": true });
    let (status, body) = call_as(app, &s.bob, "POST", "/api/v1/tasks", Some(create)).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    let me = ok_as(app, &s.bob, "GET", "/api/v1/me", None).await;
    assert_eq!(me["can_bypass"], false);

    // An unknown mode and a ghost member are refused, the saved policy kept.
    for bad in [
        json!({ "mode": "sometimes" }),
        json!({ "mode": "members", "member_ids": ["00000000000000000000000009"] }),
        json!({ "mode": "members", "member_ids": ["not-an-id"] }),
    ] {
        let (status, _) = call(app, "PUT", "/api/v1/settings/bypass", Some(bad)).await;
        assert!(status.is_client_error(), "{status}");
    }
    let policy = ok(app, "GET", "/api/v1/settings/bypass", None).await;
    assert_eq!(policy["mode"], "off");
}
