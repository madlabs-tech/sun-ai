//! End to end through the real engine, queue and store: REST + auth, the SSE
//! timeline, and the `/host` WebSocket driven by a scripted host.
#![allow(clippy::unwrap_used, clippy::expect_used)] // test helpers

use crate::common;

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use base64::Engine as _;
use common::{SECRET, TOKEN, kernel, kernel_with_mods};
use futures_util::{SinkExt, StreamExt};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use sun_core::model::RuntimeSource;
use sun_core::{HostId, ModCall, ModLink, Result, RunId, WorkspaceId};
use sun_store::{MemberRepo, RuntimeRepo, SecretRepo};
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};
use tower::ServiceExt;

const MAIN: WorkspaceId = WorkspaceId::MAIN;

const APPROVAL: &str = "
name: gatekept
steps:
  - id: approve
    type: approval
    who: [role:admin]
";

const AGENT: &str = "
name: agentic
steps:
  - id: work
    type: agent
    prompt: say hi
";

async fn call(app: &Router, method: &str, path: &str, body: Option<Value>) -> (StatusCode, Value) {
    let request = Request::builder()
        .method(method)
        .uri(path)
        .header(header::AUTHORIZATION, format!("Bearer {TOKEN}"))
        .header(header::CONTENT_TYPE, "application/json")
        .body(body.map_or(Body::empty(), |b| Body::from(b.to_string())))
        .unwrap();
    let response = common::respond(app, request).await;
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

/// Polls the task until its workspace status is `status`.
async fn until(app: &Router, task: &str, status: &str) -> Value {
    for _ in 0..200 {
        let (_, view) = call(app, "GET", &format!("/api/v1/tasks/{task}"), None).await;
        if view["status"] == status {
            return view;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("task {task} never reached {status}");
}

async fn until_online(app: &Router, host: HostId) {
    for _ in 0..500 {
        let (_, hosts) = call(app, "GET", "/api/v1/hosts", None).await;
        if hosts
            .as_array()
            .unwrap()
            .iter()
            .any(|h| h["id"] == json!(host) && h["status"] == "online")
        {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("host {host} never came online");
}

async fn new_task(app: &Router, yaml: &str) -> Value {
    let (status, workflow) = call(
        app,
        "POST",
        "/api/v1/workflows",
        Some(json!({ "source_yaml": yaml })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{workflow}");
    let (status, task) = call(
        app,
        "POST",
        "/api/v1/tasks",
        Some(json!({ "title": "Fix it", "body": "", "workflow_id": workflow["id"] })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{task}");
    task
}

#[tokio::test]
async fn builder_validates_loads_and_installs_templates() {
    let k = kernel().await;
    let app = &k.app;

    // Live validation: line:col for every problem, nothing saved.
    let bad =
        "name: x\nsteps:\n  - id: a\n    type: agent\n  - id: a\n    type: wait\n    for: soon\n";
    let (status, check) = call(
        app,
        "POST",
        "/api/v1/workflows/validate",
        Some(json!({ "source_yaml": bad })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let errors = check["errors"].as_array().unwrap();
    assert!(errors.len() >= 2, "{check}");
    assert!(errors.iter().all(|e| e["line"].as_u64() > Some(0)));
    let (_, check) = call(
        app,
        "POST",
        "/api/v1/workflows/validate",
        Some(json!({ "source_yaml": APPROVAL })),
    )
    .await;
    assert_eq!(check, json!({ "errors": [] }));

    // Templates: listed with their mods, installed once as version 1.
    let (_, templates) = call(app, "GET", "/api/v1/templates", None).await;
    let bug_fix = templates
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["id"] == "bug-fix")
        .unwrap();
    assert_eq!(bug_fix["mods"], json!(["github"]));
    let (status, installed) = call(app, "POST", "/api/v1/templates/bug-fix/install", None).await;
    assert_eq!(status, StatusCode::CREATED, "{installed}");
    assert_eq!(
        (&installed["name"], &installed["version"]),
        (&json!("bug-fix"), &json!(1))
    );
    let (status, _) = call(app, "POST", "/api/v1/templates/bug-fix/install", None).await;
    assert_eq!(status, StatusCode::CONFLICT);
    let (status, _) = call(app, "POST", "/api/v1/templates/nope/install", None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // The builder loads the latest YAML of a workflow.
    let id = installed["id"].as_str().unwrap();
    let (status, workflow) = call(app, "GET", &format!("/api/v1/workflows/{id}"), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(workflow["source_yaml"], bug_fix["source_yaml"]);
    assert_eq!(workflow["version"], 1);

    // Dry run: steps in order with what they use, mods, no cost history yet.
    let (status, dry) = call(
        app,
        "POST",
        "/api/v1/workflows/dry-run",
        Some(json!({ "source_yaml": bug_fix["source_yaml"] })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{dry}");
    let waves: Vec<(&str, u64)> = dry["steps"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| (s["id"].as_str().unwrap(), s["wave"].as_u64().unwrap()))
        .collect();
    assert_eq!(
        waves,
        [
            ("reproduce", 1),
            ("fix", 2),
            ("verify", 3),
            ("pr", 4),
            ("review", 5)
        ]
    );
    assert_eq!(dry["steps"][3]["uses"], json!(["github.open_pr"]));
    assert_eq!(
        dry["mods"],
        json!([{ "name": "github", "installed": false }])
    );
    assert_eq!(dry["agent_steps"], 2);
    assert_eq!(dry["estimated_cost_usd"], Value::Null);
    // The steps actually ran on the scripted fake runtime; the PR was
    // recorded, not sent, and the real mods saw nothing.
    let sim = &dry["simulation"];
    assert_eq!(sim["status"], "succeeded", "{sim}");
    let ran: Vec<(&str, &str)> = sim["steps"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| (s["id"].as_str().unwrap(), s["status"].as_str().unwrap()))
        .collect();
    assert_eq!(
        ran,
        [
            ("reproduce", "succeeded"),
            ("fix", "succeeded"),
            ("verify", "succeeded"),
            ("pr", "succeeded"),
            ("review", "succeeded")
        ]
    );
    assert_eq!(sim["actions"], json!(["github.open_pr"]));
    let (_, tasks) = call(app, "GET", "/api/v1/tasks", None).await;
    assert_eq!(tasks, json!([]), "a dry run creates no task");
    let (status, _) = call(
        app,
        "POST",
        "/api/v1/workflows/dry-run",
        Some(json!({ "source_yaml": "name: x\n" })),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);

    // A new version that changes `name:` renames the workflow.
    let renamed = bug_fix["source_yaml"]
        .as_str()
        .unwrap()
        .replace("name: bug-fix", "name: bug-squash");
    let path = format!("/api/v1/workflows/{id}/versions");
    let (status, saved) = call(app, "POST", &path, Some(json!({ "source_yaml": renamed }))).await;
    assert_eq!(status, StatusCode::CREATED, "{saved}");
    assert_eq!(
        (&saved["name"], &saved["version"]),
        (&json!("bug-squash"), &json!(2))
    );
    let (_, list) = call(app, "GET", "/api/v1/workflows", None).await;
    assert!(
        list.as_array()
            .unwrap()
            .iter()
            .any(|w| w["name"] == "bug-squash")
    );
}

/// Priority at create, and a webhook trigger once per `key`.
#[tokio::test]
async fn tasks_take_a_priority_and_webhooks_trigger_workflows() {
    let k = kernel().await;
    let app = &k.app;
    let hooked = APPROVAL.replace("name: gatekept", "name: hooked\non: [webhook]");
    let (_, workflow) = call(
        app,
        "POST",
        "/api/v1/workflows",
        Some(json!({ "source_yaml": hooked })),
    )
    .await;
    let id = workflow["id"].as_str().unwrap();
    let create =
        |priority: Value| json!({ "title": "Urgent", "workflow_id": id, "priority": priority });
    let (status, task) = call(app, "POST", "/api/v1/tasks", Some(create(json!("urgent")))).await;
    assert_eq!(
        (status, task["priority"].clone()),
        (StatusCode::CREATED, json!("urgent"))
    );
    for bad in [json!(7), json!("huge")] {
        let (status, _) = call(app, "POST", "/api/v1/tasks", Some(create(bad))).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    }

    let trigger = format!("/api/v1/workflows/{id}/trigger");
    let hook = json!({ "title": "Deploy failed", "key": "build-77" });
    let (status, first) = call(app, "POST", &trigger, Some(hook.clone())).await;
    assert_eq!(status, StatusCode::CREATED, "{first}");
    assert_eq!(first["title"], "Deploy failed");
    let (status, again) = call(app, "POST", &trigger, Some(hook)).await;
    assert_eq!((status, &again["id"]), (StatusCode::OK, &first["id"]));
    let (status, _) = call(app, "POST", &trigger, Some(json!([1]))).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);

    let (_, manual) = call(
        app,
        "POST",
        "/api/v1/workflows",
        Some(json!({ "source_yaml": APPROVAL })),
    )
    .await;
    let path = format!(
        "/api/v1/workflows/{}/trigger",
        manual["id"].as_str().unwrap()
    );
    let (status, error) = call(app, "POST", &path, Some(json!({}))).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{error}");
}

#[tokio::test]
async fn rest_auth_validation_approve_cancel_bump() {
    let k = kernel().await;
    let app = &k.app;

    // Auth: no token, wrong cookie → 401; sign-in (the first-run owner,
    // with the token as the password) sets the cookie.
    let anonymous = Request::get("/api/v1/tasks").body(Body::empty()).unwrap();
    let response = app.clone().oneshot(anonymous).await.unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    k.api.seed_owner().await.unwrap();
    let mut tx = k.store.begin().await.unwrap();
    let owner = tx.members().await.unwrap()[0].id;
    tx.set_member_email(owner, "owner@corp.io").await.unwrap();
    // Kept the token as their own password (the kernel makes them change it).
    let hash = tx.password_hash(owner).await.unwrap();
    tx.set_password(owner, hash.as_deref(), false)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    let sign_in = Request::post("/api/v1/session")
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(
            json!({ "email": "owner@corp.io", "password": TOKEN }).to_string(),
        ))
        .unwrap();
    let response = app.clone().oneshot(sign_in).await.unwrap();
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
    let cookie = response.headers()[header::SET_COOKIE].to_str().unwrap();
    // A session, never the admin token itself.
    assert!(
        !cookie.contains(TOKEN) && cookie.contains("HttpOnly"),
        "{cookie}"
    );
    let session = cookie.split(';').next().unwrap().to_owned();
    let with_cookie = Request::get("/api/v1/tasks")
        .header(header::COOKIE, session)
        .body(Body::empty())
        .unwrap();
    assert_eq!(
        app.clone().oneshot(with_cookie).await.unwrap().status(),
        StatusCode::OK
    );

    // Validation: plain-language 422s, bad ids, unknown fields.
    let (status, error) = call(
        app,
        "POST",
        "/api/v1/tasks",
        Some(json!({ "title": " ", "workflow_id": "x" })),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(error["error"]["code"], "invalid_input");
    let (status, _) = call(app, "GET", "/api/v1/tasks/not-a-ulid", None).await;
    assert_eq!(
        status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "neither an id nor a key"
    );
    let (status, _) = call(
        app,
        "POST",
        "/api/v1/workflows",
        Some(json!({ "source_yaml": "name: x\nsteps: []\n" })),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    let (status, _) = call(
        app,
        "POST",
        "/api/v1/workflows",
        Some(json!({ "yaml": APPROVAL })),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);

    // Saving a workflow again makes version 2.
    let (_, created) = call(
        app,
        "POST",
        "/api/v1/workflows",
        Some(json!({ "source_yaml": APPROVAL })),
    )
    .await;
    assert_eq!(created["version"], 1);
    let path = format!(
        "/api/v1/workflows/{}/versions",
        created["id"].as_str().unwrap()
    );
    let (status, saved) = call(app, "POST", &path, Some(json!({ "source_yaml": APPROVAL }))).await;
    assert_eq!(
        (status, saved["version"].clone()),
        (StatusCode::CREATED, json!(2))
    );

    // Create → queued → the approval step waits for a human.
    let task = new_task(app, APPROVAL).await;
    let id = task["id"].as_str().unwrap();
    assert!(task["key"].as_str().unwrap().starts_with("MAIN-"));
    let view = until(app, id, "needs_you").await;
    assert_eq!(view["run"]["steps"][0]["type"], "approval");
    let run_id = view["run"]["id"].as_str().unwrap().to_string();
    let (_, listed) = call(app, "GET", "/api/v1/tasks?status=needs_you", None).await;
    assert_eq!(listed.as_array().unwrap().len(), 1);

    // Runtime switch: only a runtime of the chain, only an agent step.
    let switch = format!("/api/v1/runs/{run_id}/steps/approve/switch");
    let (status, error) = call(app, "POST", &switch, Some(json!({ "to": "nope" }))).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{error}");
    assert!(
        error["error"]["message"]
            .as_str()
            .unwrap()
            .contains("not in the chain")
    );
    let (status, error) = call(app, "POST", &switch, Some(json!({ "to": "fake" }))).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{error}");
    let (status, _) = call(app, "POST", &switch, Some(json!({ "to": " " }))).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    let unknown = format!("/api/v1/runs/{}/steps/approve/switch", RunId::generate());
    let (status, _) = call(app, "POST", &unknown, Some(json!({ "to": "fake" }))).await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // Limit policy per task and per ecosystem.
    let on_limit = format!("/api/v1/tasks/{id}/on_limit");
    let pause = json!({ "on_limit": "pause_and_notify" });
    let (status, set) = call(app, "PUT", &on_limit, Some(pause.clone())).await;
    assert_eq!((status, set), (StatusCode::OK, pause.clone()));
    let (_, read) = call(app, "GET", &on_limit, None).await;
    assert_eq!(read, pause);
    let bad = json!({ "on_limit": "explode" });
    let (status, _) = call(app, "PUT", &on_limit, Some(bad)).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    let wait = json!({ "on_limit": "wait_until_reset" });
    let (status, _) = call(app, "PUT", "/api/v1/settings/on_limit", Some(wait.clone())).await;
    assert_eq!(status, StatusCode::OK);
    let (_, ecosystem) = call(app, "GET", "/api/v1/settings/on_limit", None).await;
    assert_eq!(ecosystem, wait);

    let (status, bumped) = call(
        app,
        "POST",
        &format!("/api/v1/tasks/{id}/bump"),
        Some(json!({ "priority": 5 })),
    )
    .await;
    assert_eq!(
        (status, bumped["priority"].clone()),
        (StatusCode::OK, json!("high")),
        "the old integer maps to the enum"
    );

    let (status, done) = call(
        app,
        "POST",
        &format!("/api/v1/tasks/{id}/approve"),
        Some(json!({})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{done}");
    assert_eq!(done["status"], "done");
    assert_eq!(done["run"]["end_reason"], "approved");
    let (status, _) = call(
        app,
        "POST",
        &format!("/api/v1/tasks/{id}/approve"),
        Some(json!({})),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);

    // The ledger, with a summary line on every event.
    let (_, events) = call(
        app,
        "GET",
        &format!("/api/v1/runs/{run_id}/events?after=0"),
        None,
    )
    .await;
    let events = events.as_array().unwrap();
    assert!(
        events
            .iter()
            .any(|e| e["kind"] == "approval" && e["payload"]["summary"] == "Approved by me")
    );
    let last = events.last().unwrap()["seq"].as_i64().unwrap();
    let (_, tail) = call(
        app,
        "GET",
        &format!("/api/v1/runs/{run_id}/events?after={last}"),
        None,
    )
    .await;
    assert_eq!(tail, json!([]));

    // A rejection keeps its reason on the approval event.
    let rejected = new_task(app, APPROVAL).await;
    let rejected_id = rejected["id"].as_str().unwrap();
    until(app, rejected_id, "needs_you").await;
    let path = format!("/api/v1/tasks/{rejected_id}/reject");
    let long = "x".repeat(2001);
    let (status, _) = call(app, "POST", &path, Some(json!({ "reason": long }))).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    let (status, view) = call(app, "POST", &path, Some(json!({ "reason": " off-brand " }))).await;
    assert_eq!(status, StatusCode::OK, "{view}");
    assert_eq!(view["run"]["end_reason"], "rejected");
    let run = view["run"]["id"].as_str().unwrap();
    let (_, events) = call(
        app,
        "GET",
        &format!("/api/v1/runs/{run}/events?after=0"),
        None,
    )
    .await;
    let approval = events
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["kind"] == "approval")
        .unwrap();
    assert_eq!(approval["payload"]["reason"], "off-brand");

    // Cancel a second task; a cancelled task reopens to Todo with a new run.
    let other = new_task(app, APPROVAL).await;
    let other_id = other["id"].as_str().unwrap();
    until(app, other_id, "needs_you").await;
    let (status, cancelled) = call(
        app,
        "POST",
        &format!("/api/v1/tasks/{other_id}/cancel"),
        Some(json!({})),
    )
    .await;
    assert_eq!(
        (status, cancelled["status"].clone()),
        (StatusCode::OK, json!("cancelled"))
    );
    let other_run = cancelled["run"]["id"].as_str().unwrap();
    let (status, retried) = call(
        app,
        "POST",
        &format!("/api/v1/runs/{other_run}/retry"),
        Some(json!({})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{retried}");
    assert_ne!(retried["run"]["id"], json!(other_run), "a new run");
    until(app, other_id, "needs_you").await;

    // Someone not in `who:` may not decide: 403, and the step keeps waiting.
    let boss_only = APPROVAL.replace("role:admin", "user:boss");
    let guarded = new_task(app, &boss_only).await;
    let guarded_id = guarded["id"].as_str().unwrap();
    until(app, guarded_id, "needs_you").await;
    let path = format!("/api/v1/tasks/{guarded_id}/approve");
    let (status, error) = call(app, "POST", &path, Some(json!({}))).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{error}");
    assert_eq!(error["error"]["code"], "forbidden");
    until(app, guarded_id, "needs_you").await;

    let (_, usage) = call(app, "GET", "/api/v1/usage", None).await;
    assert_eq!(usage["cost_usd"], 0.0);
    let (_, workflows) = call(app, "GET", "/api/v1/workflows", None).await;
    assert_eq!(workflows.as_array().unwrap().len(), 5);
    let (_, mods) = call(app, "GET", "/api/v1/mods", None).await;
    assert_eq!(mods, json!([]));
    // No installer wired (sun-cli wires one): installing from the UI is 503.
    let body = json!({ "permissions": [] });
    let (status, _) = call(app, "POST", "/api/v1/mods/github/install", Some(body)).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    let (status, _) = call(app, "DELETE", "/api/v1/mods/github", None).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    let (status, _) = call(app, "DELETE", "/api/v1/mods/..%2Fx", None).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    let install = sun_core::model::ModInstall {
        name: "github".into(),
        kind: sun_core::model::ModKind::Plugin,
        version: "1.0.0".into(),
        manifest: json!({}),
        granted_permissions: vec!["secret:GITHUB_TOKEN".into()],
        package_digest: None,
        signed_by: Some("ab".into()),
    };
    let mut tx = k.store.begin().await.unwrap();
    sun_store::ModInstallRepo::put_mod_install(&mut tx, &install)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    let (_, mods) = call(app, "GET", "/api/v1/mods", None).await;
    assert_eq!(
        mods,
        json!([{ "name": "github", "kind": "plugin", "version": "1.0.0", "installed": true,
                 "permissions": ["secret:GITHUB_TOKEN"], "signed": true,
                 "needed_by": [], "update": null }])
    );
}

type Socket = WebSocketStream<MaybeTlsStream<TcpStream>>;

async fn send(socket: &mut Socket, frame: Value) {
    socket
        .send(Message::Text(frame.to_string().into()))
        .await
        .unwrap();
}

/// The next kernel frame, or `None` once the kernel closed the socket.
async fn recv(socket: &mut Socket) -> Option<Value> {
    loop {
        let message = tokio::time::timeout(Duration::from_secs(10), socket.next())
            .await
            .expect("kernel went quiet");
        match message {
            Some(Ok(Message::Text(text))) => return Some(serde_json::from_str(&text).unwrap()),
            Some(Ok(Message::Close(_))) | None | Some(Err(_)) => return None,
            _ => {}
        }
    }
}

fn hello(host: HostId, key: &str) -> Value {
    json!({
        "type": "hello", "host_id": host, "host_key": key, "protocol": "2.0",
        "capabilities": { "os": "linux", "arch": "x86_64", "cpus": 4, "runtimes": [
            { "adapter": "fake", "readiness": { "installed": true, "version": "1", "logged_in": true, "detail": null } }
        ], "exec_envs": [
            { "kind": "bare", "ready": true, "version": null, "detail": null },
            { "kind": "container", "ready": true, "version": "Docker version 28.5.1", "detail": null },
            { "kind": "vm", "ready": false, "version": null, "detail": "install Lima" }
        ] },
        "last_ack": {},
    })
}

/// One `status`/`leave` connection: the kernel's answer, or `None` when it
/// closed without one.
async fn ask(url: &str, frame: Value) -> Option<Value> {
    let (mut socket, _) = tokio_tungstenite::connect_async(url).await.unwrap();
    send(&mut socket, frame).await;
    let answer = recv(&mut socket).await;
    if answer.is_some() {
        assert_eq!(recv(&mut socket).await, None, "one answer, then close");
    }
    answer
}

#[tokio::test]
async fn host_socket_status_and_leave_need_the_host_key() {
    let k = kernel().await;
    let app = &k.app;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("ws://{}/host", listener.local_addr().unwrap());
    tokio::spawn(axum::serve(listener, app.clone()).into_future());
    let main = "/api/v1/workspaces/00000000000000000000000001/hosts/tokens";
    let (_, issued) = call(app, "POST", main, None).await;
    // The UI renders `sun host join --kernel <kernel_url>` from it.
    assert_eq!(issued["kernel_url"], "http://localhost");
    let enroll = json!({ "type": "enroll", "token": issued["token"], "name": "laptop" });
    let enrolled = ask(&url, enroll).await.unwrap();
    let host: HostId = serde_json::from_value(enrolled["host_id"].clone()).unwrap();
    let key = enrolled["host_key"].as_str().unwrap();
    let as_host = |kind: &str, key: &str| json!({ "type": kind, "host_id": host, "host_key": key });

    // A wrong key, or a host this kernel never enrolled, gets no answer.
    assert_eq!(ask(&url, as_host("status", "wrong")).await, None);
    assert_eq!(ask(&url, as_host("leave", "wrong")).await, None);
    let stranger = json!({ "type": "status", "host_id": HostId::generate(), "host_key": key });
    assert_eq!(ask(&url, stranger).await, None);

    let status = ask(&url, as_host("status", key)).await.unwrap();
    assert_eq!(
        status,
        json!({ "type": "status", "connected": false, "workspaces": ["Main"] })
    );
    let (mut session, _) = tokio_tungstenite::connect_async(&url).await.unwrap();
    send(&mut session, hello(host, key)).await;
    until_online(app, host).await;
    let status = ask(&url, as_host("status", key)).await.unwrap();
    assert_eq!(status["connected"], true);

    // Leaving takes it out of every workspace's list, audited as the host.
    let listed = |hosts: &Value| {
        hosts
            .as_array()
            .unwrap()
            .iter()
            .any(|h| h["id"] == json!(host))
    };
    let in_main = "/api/v1/hosts?workspace=00000000000000000000000001";
    assert!(listed(&call(app, "GET", in_main, None).await.1));
    let left = ask(&url, as_host("leave", key)).await.unwrap();
    assert_eq!(left, json!({ "type": "left" }));
    assert!(!listed(&call(app, "GET", in_main, None).await.1));
    let status = ask(&url, as_host("status", key)).await.unwrap();
    assert_eq!(status["workspaces"], json!([]));
    let (_, audit) = call(app, "GET", "/api/v1/audit", None).await;
    assert!(
        audit.to_string().contains(&format!("host:{host}")),
        "{audit}"
    );
    // Leaving twice is fine.
    assert_eq!(
        ask(&url, as_host("leave", key)).await,
        Some(json!({ "type": "left" }))
    );
}

#[tokio::test]
async fn host_socket_enrolment_session_sse_and_steer() {
    let k = kernel().await;
    let app = &k.app;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("ws://{}/host", listener.local_addr().unwrap());
    tokio::spawn(axum::serve(listener, app.clone()).into_future());

    // Enrolment: a token works once.
    let (_, issued) = call(
        app,
        "POST",
        "/api/v1/workspaces/00000000000000000000000001/hosts/tokens",
        None,
    )
    .await;
    let token = issued["token"].as_str().unwrap();
    let (mut socket, _) = tokio_tungstenite::connect_async(&url).await.unwrap();
    send(
        &mut socket,
        json!({ "type": "enroll", "token": token, "name": "laptop" }),
    )
    .await;
    let enrolled = recv(&mut socket).await.unwrap();
    assert_eq!(enrolled["type"], "enrolled");
    assert_eq!(recv(&mut socket).await, None);
    let (mut socket, _) = tokio_tungstenite::connect_async(&url).await.unwrap();
    send(
        &mut socket,
        json!({ "type": "enroll", "token": token, "name": "again" }),
    )
    .await;
    assert_eq!(recv(&mut socket).await, None);

    // A wrong key is closed; the derived key (sha256(secret:host_id)) is accepted.
    let (mut socket, _) = tokio_tungstenite::connect_async(&url).await.unwrap();
    send(&mut socket, hello(k.host, "wrong")).await;
    assert_eq!(recv(&mut socket).await, None);
    let key: String = Sha256::digest(format!("{SECRET}:{}", k.host).as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    let (mut host, _) = tokio_tungstenite::connect_async(&url).await.unwrap();
    send(&mut host, hello(k.host, &key)).await;
    until_online(app, k.host).await;
    let (_, runtimes) = call(app, "GET", "/api/v1/runtimes", None).await;
    assert_eq!(runtimes[0]["state"], "idle");
    assert_eq!(runtimes[0]["permission_mode"], Value::Null, "host default");
    let (_, hosts) = call(app, "GET", "/api/v1/hosts", None).await;
    let envs = &hosts[0]["capabilities"]["exec_envs"];
    assert_eq!(
        (envs[1]["kind"].clone(), envs[1]["ready"].clone()),
        (json!("container"), json!(true))
    );
    assert_eq!(envs[2]["detail"], "install Lima");

    // A permission mode per runtime: validated, bypass refused without the
    // operator's opt-in, audited, listed and sent with every session.
    let adapter = runtimes[0]["tool"].as_str().unwrap().to_owned();
    let policy = format!("/api/v1/runtimes/{adapter}/policy");
    for bad in ["yolo", "acceptEdits", "bypass_permissions"] {
        let body = json!({ "permission_mode": bad });
        let (status, error) = call(app, "PUT", &policy, Some(body)).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{error}");
    }
    let body = json!({ "permission_mode": "plan" });
    let (status, _) = call(app, "PUT", &policy, Some(body)).await;
    assert_eq!(status, StatusCode::OK);
    let (_, runtimes) = call(app, "GET", "/api/v1/runtimes", None).await;
    assert_eq!(runtimes[0]["permission_mode"], "plan");
    let (_, audit) = call(app, "GET", "/api/v1/audit", None).await;
    assert!(audit.to_string().contains("policy_change"), "{audit}");

    // The engine starts the agent session on this host, in the task's
    // exec env (its override wins over the workflow's `exec_env:`).
    let yaml = AGENT.replace(
        "steps:",
        "exec_env: { kind: vm, template: default }\nsteps:",
    );
    let (status, workflow) = call(
        app,
        "POST",
        "/api/v1/workflows",
        Some(json!({ "source_yaml": yaml })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{workflow}");
    let create = |exec_env: Value| json!({ "title": "Fix it", "workflow_id": workflow["id"], "exec_env": exec_env });
    let bad = create(json!({ "kind": "container", "image": "--privileged" }));
    let (status, _) = call(app, "POST", "/api/v1/tasks", Some(bad)).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    let container = json!({ "kind": "container", "image": "rust:1.90", "network": "none" });
    let (status, task) = call(app, "POST", "/api/v1/tasks", Some(create(container))).await;
    assert_eq!(status, StatusCode::CREATED, "{task}");
    // The task's folder first; the session starts in the returned path.
    let prepare = recv(&mut host).await.unwrap();
    assert_eq!(prepare["type"], "worktree.prepare");
    assert_eq!(prepare["worktree"]["kind"], "folder");
    send(
        &mut host,
        json!({ "type": "reply", "id": prepare["id"], "body": { "kind": "worktree", "path": "/data/work/t" } }),
    )
    .await;
    // Then its context files, then the session.
    let write = recv(&mut host).await.unwrap();
    assert_eq!(write["type"], "worktree.write");
    assert_eq!(write["cwd"], "/data/work/t");
    assert!(
        write["files"]
            .as_array()
            .unwrap()
            .iter()
            .any(|f| f["path"] == ".sun/context/pack.md")
    );
    send(
        &mut host,
        json!({ "type": "reply", "id": write["id"], "body": { "kind": "done" } }),
    )
    .await;
    let start = recv(&mut host).await.unwrap();
    assert_eq!(start["type"], "session.start");
    assert_eq!(start["spec"]["worktree"], "/data/work/t");
    assert_eq!(start["spec"]["permission_mode"], "plan");
    assert_eq!(start["spec"]["exec_env"]["kind"], "container");
    assert_eq!(start["spec"]["exec_env"]["image"], "rust:1.90");
    let session_id = start["spec"]["session_id"].clone();
    send(
        &mut host,
        json!({ "type": "reply", "id": start["id"], "body": {
        "kind": "session", "handle": { "session_id": session_id, "native_id": null, "pid": 7 } } }),
    )
    .await;
    let view = until(app, task["id"].as_str().unwrap(), "in_progress").await;
    let run_id = view["run"]["id"].as_str().unwrap().to_string();
    let (_, events) = call(app, "GET", &format!("/api/v1/runs/{run_id}/events"), None).await;
    assert!(
        events
            .to_string()
            .contains("Session on fake · your subscription · container rust:1.90"),
        "{events}"
    );

    // SSE from the start of the run: history, then live events with ids.
    let request = Request::get(format!("/api/v1/runs/{run_id}/events?after=0"))
        .header(header::AUTHORIZATION, format!("Bearer {TOKEN}"))
        .header(header::ACCEPT, "text/event-stream")
        .body(Body::empty())
        .unwrap();
    let response = app.clone().oneshot(request).await.unwrap();
    assert_eq!(
        response.headers()[header::CONTENT_TYPE],
        "text/event-stream"
    );
    let mut body = response.into_body();

    send(
        &mut host,
        json!({ "type": "event", "session_id": session_id, "seq": 1,
        "event": { "kind": "text", "role": "assistant", "text": "hi" } }),
    )
    .await;
    let ack = recv(&mut host).await.unwrap();
    assert_eq!(
        ack,
        json!({ "type": "ack", "session_id": session_id, "seq": 1 })
    );
    // A redelivery is stored once and acked again.
    send(
        &mut host,
        json!({ "type": "event", "session_id": session_id, "seq": 1,
        "event": { "kind": "text", "role": "assistant", "text": "hi" } }),
    )
    .await;

    let mut seen = String::new();
    while !seen.contains("\"text\":\"hi\"") {
        let frame = tokio::time::timeout(Duration::from_secs(10), body.frame())
            .await
            .expect("no SSE frame")
            .unwrap()
            .unwrap();
        if let Ok(data) = frame.into_data() {
            seen.push_str(std::str::from_utf8(&data).unwrap());
        }
    }
    assert!(seen.contains("id: 1\n"), "{seen}");
    assert_eq!(seen.matches("\"text\":\"hi\"").count(), 1, "{seen}");

    // Usage events add up into the run's cost.
    for seq in [2, 3] {
        send(&mut host, json!({ "type": "event", "session_id": session_id, "seq": seq,
            "event": { "kind": "usage", "input": 10, "output": 5, "cost_usd": 0.25, "context_used": null } })).await;
    }
    for _ in 0..100 {
        let (_, usage) = call(app, "GET", &format!("/api/v1/usage?run_id={run_id}"), None).await;
        if usage == json!({ "input_tokens": 20, "output_tokens": 10, "cost_usd": 0.5 }) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let (_, view) = call(
        app,
        "GET",
        &format!("/api/v1/tasks/{}", task["id"].as_str().unwrap()),
        None,
    )
    .await;
    assert_eq!(view["run"]["cost_usd"], 0.5);

    // Steering goes to the host as session.send and lands in the ledger.
    let steer_app = app.clone();
    let steer_path = format!("/api/v1/runs/{run_id}/messages");
    let steering = tokio::spawn(async move {
        call(
            &steer_app,
            "POST",
            &steer_path,
            Some(json!({ "body": "use tabs" })),
        )
        .await
    });
    let mut frame = recv(&mut host).await.unwrap();
    while frame["type"] == "ack" {
        frame = recv(&mut host).await.unwrap();
    }
    assert_eq!(frame["type"], "session.send");
    assert_eq!(frame["text"], "use tabs");
    send(
        &mut host,
        json!({ "type": "reply", "id": frame["id"], "body": { "kind": "done" } }),
    )
    .await;
    let (status, event) = steering.await.unwrap();
    assert_eq!(status, StatusCode::OK, "{event}");
    assert_eq!(event["kind"], "intervention");
    assert_eq!(event["payload"]["summary"], "You: use tabs");

    // Pause kills the session; resume starts a new one in the same folder.
    let task_path = format!("/api/v1/tasks/{}", task["id"].as_str().unwrap());
    let pause_app = app.clone();
    let pause_path = format!("{task_path}/pause");
    let pausing =
        tokio::spawn(async move { call(&pause_app, "POST", &pause_path, Some(json!({}))).await });
    let stop = recv(&mut host).await.unwrap();
    assert_eq!(
        (stop["type"].clone(), stop["how"].clone()),
        (json!("session.stop"), json!("kill"))
    );
    send(
        &mut host,
        json!({ "type": "reply", "id": stop["id"], "body": { "kind": "done" } }),
    )
    .await;
    let (status, paused) = pausing.await.unwrap();
    assert_eq!(status, StatusCode::OK, "{paused}");
    assert_eq!(
        paused["run"]["status"], "waiting_approval",
        "paused shows as needing a human"
    );
    let (status, _) = call(app, "POST", &format!("{task_path}/pause"), None).await;
    assert_eq!(status, StatusCode::CONFLICT, "already paused");

    let resume_app = app.clone();
    let resume_path = format!("{task_path}/resume");
    let resuming =
        tokio::spawn(async move { call(&resume_app, "POST", &resume_path, Some(json!({}))).await });
    let prepare = recv(&mut host).await.unwrap();
    assert_eq!(prepare["type"], "worktree.prepare");
    send(&mut host, json!({ "type": "reply", "id": prepare["id"], "body": { "kind": "worktree", "path": "/data/work/t" } })).await;
    // A host that can't write the files: the whole pack rides in the envelope.
    let write = recv(&mut host).await.unwrap();
    assert_eq!(write["type"], "worktree.write");
    send(
        &mut host,
        json!({ "type": "reply", "id": write["id"], "body": {
        "kind": "error", "code": "unavailable", "message": "disk full" } }),
    )
    .await;
    let start = recv(&mut host).await.unwrap();
    assert_eq!(start["type"], "session.start");
    assert_ne!(start["spec"]["session_id"], session_id, "a new session");
    let envelope = start["spec"]["envelope"].as_str().unwrap();
    assert!(envelope.contains("## Task"), "{envelope}");
    send(&mut host, json!({ "type": "reply", "id": start["id"], "body": {
        "kind": "session", "handle": { "session_id": start["spec"]["session_id"], "native_id": null, "pid": 8 } } })).await;
    let (status, resumed) = resuming.await.unwrap();
    assert_eq!(status, StatusCode::OK, "{resumed}");
    assert_eq!(resumed["run"]["status"], "running");

    // Cancel stops the session on the host.
    let cancel_app = app.clone();
    let cancel_path = format!("/api/v1/tasks/{}/cancel", task["id"].as_str().unwrap());
    let cancelling =
        tokio::spawn(async move { call(&cancel_app, "POST", &cancel_path, Some(json!({}))).await });
    let stop = recv(&mut host).await.unwrap();
    assert_eq!(
        (stop["type"].clone(), stop["how"].clone()),
        (json!("session.stop"), json!("kill"))
    );
    send(
        &mut host,
        json!({ "type": "reply", "id": stop["id"], "body": { "kind": "done" } }),
    )
    .await;
    let (status, cancelled) = cancelling.await.unwrap();
    assert_eq!(
        (status, cancelled["status"].clone()),
        (StatusCode::OK, json!("cancelled"))
    );
}

#[tokio::test]
async fn catalog_routes_list_suggest_show_install_and_uninstall() {
    let k = kernel().await;
    let (status, view) = call(&k.app, "GET", "/api/v1/catalog", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(view["stacks"], json!([]));
    let entries = view["entries"].as_array().unwrap();
    assert!(entries.iter().all(|e| e["installed"].is_null()));

    let repo = tempfile::tempdir().unwrap();
    std::fs::write(
        repo.path().join("package.json"),
        r#"{"dependencies":{"react":"19"}}"#,
    )
    .unwrap();
    let path = format!("/api/v1/catalog?repo={}", repo.path().display());
    let (_, view) = call(&k.app, "GET", &path, None).await;
    assert_eq!(view["stacks"], json!(["frontend", "node", "react"]));
    assert_eq!(view["entries"][0]["matches"], json!(["frontend"]));
    let (status, _) = call(&k.app, "GET", "/api/v1/catalog?repo=relative/dir", None).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);

    let (_, files) = call(&k.app, "GET", "/api/v1/catalog/tdd/files", None).await;
    assert!(
        files
            .as_array()
            .unwrap()
            .iter()
            .any(|f| f["path"] == "SKILL.md")
    );

    let (status, _) = call(
        &k.app,
        "POST",
        "/api/v1/catalog/tdd/install",
        Some(json!({"runtimes": []})),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "at least one runtime"
    );
    let (status, _) = call(
        &k.app,
        "POST",
        "/api/v1/catalog/tdd/install",
        Some(json!({"runtimes": ["claude-code"]})),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (_, view) = call(&k.app, "GET", "/api/v1/catalog", None).await;
    let tdd = view["entries"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["id"] == "tdd")
        .unwrap()
        .clone();
    assert_eq!(tdd["installed"], tdd["version"]);
    assert_eq!(tdd["runtimes"], json!(["claude-code"]));

    let (status, _) = call(&k.app, "DELETE", "/api/v1/catalog/tdd", None).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (status, _) = call(&k.app, "DELETE", "/api/v1/catalog/tdd", None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _) = call(
        &k.app,
        "POST",
        "/api/v1/catalog/nope/install",
        Some(json!({})),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // A skill a saved workflow uses ranks first.
    let yaml = "name: debug\nsteps:\n  - id: work\n    type: agent\n    prompt: fix it\n    skills: [diagnosing-bugs]\n";
    let (status, _) = call(
        &k.app,
        "POST",
        "/api/v1/workflows",
        Some(json!({ "source_yaml": yaml })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let (_, view) = call(&k.app, "GET", "/api/v1/catalog", None).await;
    assert_eq!(view["entries"][0]["id"], "diagnosing-bugs");
    assert_eq!(view["entries"][0]["in_workflows"], true);

    // Dismissals are the ecosystem's, not the browser's.
    assert_eq!(view["dismissed"], json!([]));
    let dismissed = "/api/v1/catalog/tdd/dismissed";
    let (status, _) = call(&k.app, "PUT", dismissed, None).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (_, view) = call(&k.app, "GET", "/api/v1/catalog", None).await;
    assert_eq!(view["dismissed"], json!(["tdd"]));
    let (status, _) = call(&k.app, "DELETE", dismissed, None).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (_, view) = call(&k.app, "GET", "/api/v1/catalog", None).await;
    assert_eq!(view["dismissed"], json!([]));
    let (status, _) = call(&k.app, "PUT", "/api/v1/catalog/Bad_Id/dismissed", None).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
}

/// Builds `sun-fake-runtime` (a no-op when fresh) and returns its path.
fn fake_binary() -> std::path::PathBuf {
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
    let built = std::process::Command::new(cargo)
        .args([
            "build",
            "-q",
            "-p",
            "sun-runtime",
            "--bin",
            "sun-fake-runtime",
        ])
        .status()
        .unwrap();
    assert!(built.success(), "building sun-fake-runtime failed");
    let exe = std::env::current_exe().unwrap();
    exe.parent()
        .unwrap()
        .parent()
        .unwrap()
        .join("sun-fake-runtime")
}

fn git(cwd: &std::path::Path, args: &[&str]) {
    let ok = std::process::Command::new("git")
        .args(["-c", "user.email=t@t", "-c", "user.name=t"])
        .args(args)
        .current_dir(cwd)
        .status()
        .unwrap();
    assert!(ok.success(), "git {args:?}");
}

/// Repo scope over the real `sun-host`: the skill lands in the
/// native folders on `sun/skill-tdd` in a bare origin and the PR goes out
/// through the outbox, once.
#[tokio::test]
async fn catalog_skill_repo_install_pushes_and_opens_a_pr() {
    let k = kernel().await;
    let app = &k.app;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("ws://{}/host", listener.local_addr().unwrap());
    tokio::spawn(axum::serve(listener, app.clone()).into_future());

    let dir = tempfile::tempdir().unwrap();
    let seed = dir.path().join("seed");
    std::fs::create_dir_all(&seed).unwrap();
    std::fs::write(seed.join("README.md"), "# web\n").unwrap();
    git(&seed, &["init", "-q", "-b", "main"]);
    git(&seed, &["add", "README.md"]);
    git(&seed, &["commit", "-q", "-m", "base"]);
    git(dir.path(), &["clone", "-q", "--bare", "seed", "origin.git"]);
    let origin = dir.path().join("origin.git");
    let host =
        sun_host::Host::open(sun_host::HostConfig::new(dir.path().join("host")), vec![]).unwrap();
    let key: String = Sha256::digest(format!("{SECRET}:{}", k.host).as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    tokio::spawn(host.run(sun_host::Credentials {
        kernel_url: url,
        host_id: k.host,
        host_key: key,
    }));
    until_online(app, k.host).await;
    let repo_id = common::local_repo(&k, MAIN, origin.to_str().unwrap()).await;
    let status_of = format!("/api/v1/catalog/tdd/repos/{repo_id}");
    let (status, _) = call(app, "GET", &status_of, None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    for (body, want) in [
        (json!({"repo_id": "nope"}), StatusCode::UNPROCESSABLE_ENTITY),
        (
            json!({"repo_id": repo_id, "runtimes": []}),
            StatusCode::UNPROCESSABLE_ENTITY,
        ),
    ] {
        let (status, _) = call(app, "POST", "/api/v1/catalog/tdd/install", Some(body)).await;
        assert_eq!(status, want);
    }
    let (status, _) = call(
        app,
        "POST",
        "/api/v1/catalog/playwright/install",
        Some(json!({"repo_id": repo_id})),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "skills only");

    let install = json!({"repo_id": repo_id, "runtimes": ["claude-code"]});
    let (status, pr) = call(
        app,
        "POST",
        "/api/v1/catalog/tdd/install",
        Some(install.clone()),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{pr}");
    assert_eq!(pr["branch"], "sun/skill-tdd");
    let show = std::process::Command::new("git")
        .args(["--git-dir", origin.to_str().unwrap()])
        .args(["show", "sun/skill-tdd:.claude/skills/tdd/SKILL.md"])
        .output()
        .unwrap();
    assert!(show.status.success(), "{show:?}");
    assert!(String::from_utf8_lossy(&show.stdout).contains("name: tdd"));

    let mut delivered = false;
    for _ in 0..500 {
        let (_, got) = call(app, "GET", &status_of, None).await;
        if got["pr"]["status"] == "delivered" {
            delivered = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(delivered, "open_pr never delivered");
    let (status, again) = call(app, "POST", "/api/v1/catalog/tdd/install", Some(install)).await;
    assert_eq!(status, StatusCode::ACCEPTED);
    assert_eq!(again["task_id"], pr["task_id"], "idempotent");
    let (_, listed) = call(app, "GET", "/api/v1/catalog", None).await;
    assert!(
        listed["entries"]
            .as_array()
            .unwrap()
            .iter()
            .all(|e| e["installed"].is_null()),
        "repo scope leaves the ecosystem store alone"
    );
    // The repo's installed view lists it.
    let (_, view) = call(
        app,
        "GET",
        &format!("/api/v1/catalog?repo_id={repo_id}"),
        None,
    )
    .await;
    let installs = view["repo_installs"].as_array().unwrap();
    assert_eq!(installs.len(), 1, "{view}");
    assert_eq!(installs[0]["skill"], "tdd");
}

/// The real `sun-host` over loopback: the engine prepares the task's repo
/// worktree, the session and the gates run in it, and the content gate's
/// file lands as a task artifact.
#[tokio::test(flavor = "multi_thread")]
async fn real_host_runs_a_repo_task_in_its_worktree() {
    let k = kernel().await;
    let app = &k.app;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("ws://{}/host", listener.local_addr().unwrap());
    tokio::spawn(axum::serve(listener, app.clone()).into_future());

    let dir = tempfile::tempdir().unwrap();
    let origin = dir.path().join("origin");
    std::fs::create_dir_all(&origin).unwrap();
    std::fs::write(origin.join("README.md"), "# Intro\nHello.\n").unwrap();
    git(&origin, &["init", "-q", "-b", "main"]);
    git(&origin, &["add", "README.md"]);
    git(&origin, &["commit", "-q", "-m", "base"]);
    let script = dir.path().join("script.jsonl");
    std::fs::write(
        &script,
        "{\"kind\":\"text\",\"role\":\"assistant\",\"text\":\"on it\"}\n\
         {\"kind\":\"done_claimed\",\"summary\":\"finished\"}\n",
    )
    .unwrap();
    let fake = sun_runtime::FakeRuntime::new("fake", fake_binary(), script);
    let host_dir = dir.path().join("host");
    let host =
        sun_host::Host::open(sun_host::HostConfig::new(&host_dir), vec![Arc::new(fake)]).unwrap();
    let key: String = Sha256::digest(format!("{SECRET}:{}", k.host).as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    tokio::spawn(host.run(sun_host::Credentials {
        kernel_url: url,
        host_id: k.host,
        host_key: key,
    }));
    until_online(app, k.host).await;

    common::local_repo(&k, MAIN, origin.to_str().unwrap()).await;
    let (status, bad) = call(
        app,
        "POST",
        &format!("/api/v1/workspaces/{MAIN}/repos"),
        Some(json!({ "url": "--upload-pack=x" })),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{bad}");

    // The gate checks the task branch `sun/<KEY>-<slug>` of "Repo origin".
    let yaml = r#"
name: repo-task
steps:
  - id: work
    type: agent
    prompt: say hi
  - id: verify
    type: gate
    gates:
      - test: {command: "git branch --show-current | grep -Eq '^sun/[A-Z0-9]+-[0-9]+-repo-origin$'"}
      - sections_present: {file: README.md, sections: [Intro]}
"#;
    let (_, workflow) = call(
        app,
        "POST",
        "/api/v1/workflows",
        Some(json!({ "source_yaml": yaml })),
    )
    .await;
    let (status, task) = call(
        app,
        "POST",
        "/api/v1/tasks",
        // The title names the repo (`origin`): the run works in it.
        Some(json!({ "title": "Repo origin", "workflow_id": workflow["id"] })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{task}");
    let task_id = task["id"].as_str().unwrap();
    let mut view = Value::Null;
    for _ in 0..1000 {
        (_, view) = call(app, "GET", &format!("/api/v1/tasks/{task_id}"), None).await;
        if !matches!(
            view["status"].as_str(),
            Some("todo" | "in_progress" | "verifying")
        ) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let run = view["run"]["id"].as_str().unwrap_or_default();
    let (_, events) = call(
        app,
        "GET",
        &format!("/api/v1/runs/{run}/events?after=0"),
        None,
    )
    .await;
    // No approval step: the run succeeded and a person reviews it.
    assert_eq!(view["status"], "needs_you", "{events:#}");
    assert_eq!(view["run"]["status"], "done", "{events:#}");
    // The gate checked the branch in the worktree; once done it is removed.
    until_gone(&host_dir.join("work").join(task_id)).await;

    // The content gate saved README.md as the task's artifact.
    assert_eq!(view["artifacts"][0]["name"], "README.md", "{view:#}");
    let artifact = view["artifacts"][0]["id"].as_str().unwrap();
    let request = Request::get(format!("/api/v1/artifacts/{artifact}"))
        .header(header::AUTHORIZATION, format!("Bearer {TOKEN}"))
        .body(Body::empty())
        .unwrap();
    let response = app.clone().oneshot(request).await.unwrap();
    assert_eq!(response.headers()[header::CONTENT_TYPE], "text/markdown");
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    assert_eq!(&bytes[..], b"# Intro\nHello.\n");
}

/// Waits until `path` is gone (the engine removes worktrees after commit).
async fn until_gone(path: &std::path::Path) {
    for _ in 0..500 {
        if !path.exists() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("{} was not removed", path.display());
}

/// A git host mod: `open_pr` answers PR 1, 2, …; `ci_status` is green;
/// `pr_merged` triggers come from the test through `merges`. The
/// `merge_order` check of the `web` PR answers once `hold` is released.
struct GitHost {
    calls: std::sync::Mutex<Vec<ModCall>>,
    merges: std::sync::Mutex<Option<tokio::sync::mpsc::UnboundedReceiver<sun_core::ModEvent>>>,
    hold: tokio::sync::Semaphore,
}

#[async_trait]
impl ModLink for GitHost {
    async fn call(&self, call: ModCall) -> Result<Value> {
        let web_check = call.method == "ci_status"
            && call.params["repo"]
                .as_str()
                .is_some_and(|r| r.ends_with("/web"))
            && call.params["ref"]
                .as_str()
                .is_some_and(|r| r.starts_with("sun/"));
        if web_check {
            drop(self.hold.acquire().await);
        }
        let mut calls = self.calls.lock().unwrap();
        calls.push(call.clone());
        Ok(match call.method.as_str() {
            "open_pr" => {
                let n = calls.iter().filter(|c| c.method == "open_pr").count();
                json!({ "number": n, "url": format!("https://git.test/pr/{n}"),
                        "head_sha": format!("abc{n}def"), "created": true })
            }
            "ci_status" => json!({ "state": "success", "checks": [] }),
            _ => json!({}),
        })
    }

    fn events(&self) -> sun_core::BoxStream<'static, sun_core::ModEvent> {
        match self.merges.lock().unwrap().take() {
            Some(rx) => Box::pin(futures_util::stream::unfold(rx, |mut rx| async move {
                rx.recv().await.map(|event| (event, rx))
            })),
            None => Box::pin(futures_util::stream::empty()),
        }
    }
}

/// Multi-repo tasks end to end on the real `sun-host`, with local bare remotes: a task on
/// three repos gets three worktrees in one session dir, gates run per repo
/// and across them, the two changed repos get a pushed branch and a PR
/// linking the other, `merge_order` waits for the merges in order, and the
/// session dir is removed when the run is done.
#[tokio::test(flavor = "multi_thread")]
async fn real_host_runs_a_multi_repo_task_with_linked_prs_in_merge_order() {
    let (merges, rx) = tokio::sync::mpsc::unbounded_channel();
    let mods = Arc::new(GitHost {
        calls: Default::default(),
        merges: std::sync::Mutex::new(Some(rx)),
        hold: tokio::sync::Semaphore::new(0),
    });
    let k = kernel_with_mods(mods.clone()).await;
    let app = &k.app;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("ws://{}/host", listener.local_addr().unwrap());
    tokio::spawn(axum::serve(listener, app.clone()).into_future());

    let dir = tempfile::tempdir().unwrap();
    // The repos' slugs, as the outbox names them (the bare path without `.git`).
    let mut slugs: Vec<String> = Vec::new();
    for name in ["api", "web", "docs"] {
        let src = dir.path().join(format!("src-{name}"));
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(src.join("README.md"), format!("# {name}\n")).unwrap();
        git(&src, &["init", "-q", "-b", "main"]);
        git(&src, &["add", "README.md"]);
        git(&src, &["commit", "-q", "-m", "base"]);
        let bare = dir.path().join(format!("{name}.git"));
        git(
            dir.path(),
            &[
                "clone",
                "-q",
                "--bare",
                src.to_str().unwrap(),
                bare.to_str().unwrap(),
            ],
        );
        common::local_repo(&k, MAIN, bare.to_str().unwrap()).await;
        let bare = bare.to_str().unwrap().trim_start_matches('/');
        slugs.push(bare.strip_suffix(".git").unwrap().to_owned());
    }
    // The agent changes api and web; docs stays as it was.
    let script = dir.path().join("script.jsonl");
    std::fs::write(
        &script,
        "{\"kind\":\"file_changed\",\"path\":\"api/change.txt\",\"write\":\"api\\n\"}\n\
         {\"kind\":\"file_changed\",\"path\":\"web/change.txt\",\"write\":\"web\\n\"}\n\
         {\"kind\":\"done_claimed\",\"summary\":\"changed api and web\"}\n",
    )
    .unwrap();
    let fake = sun_runtime::FakeRuntime::new("fake", fake_binary(), script);
    let host_dir = dir.path().join("host");
    let host =
        sun_host::Host::open(sun_host::HostConfig::new(&host_dir), vec![Arc::new(fake)]).unwrap();
    let key: String = Sha256::digest(format!("{SECRET}:{}", k.host).as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    tokio::spawn(host.run(sun_host::Credentials {
        kernel_url: url,
        host_id: k.host,
        host_key: key,
    }));
    until_online(app, k.host).await;

    let yaml = r#"
name: multi-repo
steps:
  - id: work
    type: agent
    prompt: change api and web
  - id: verify
    type: gate
    gates:
      - diff_not_empty
      - test: {command: "git branch --show-current | grep -Eq '^sun/[A-Z0-9]+-[0-9]+-ship-api-web-and-docs$'"}
      - integration: {command: "test -f api/change.txt && test -f web/change.txt && grep -q '3. `docs/`' .sun/context/pack.md"}
  - id: pr
    type: action
    use: github.open_pr
  - id: merged
    type: gate
    gates: [merge_order]
"#;
    let (_, workflow) = call(
        app,
        "POST",
        "/api/v1/workflows",
        Some(json!({ "source_yaml": yaml })),
    )
    .await;
    // The title names all three repos, in list order: api, web, docs.
    let (status, task) = call(
        app,
        "POST",
        "/api/v1/tasks",
        Some(json!({ "title": "Ship api, web and docs", "workflow_id": workflow["id"] })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{task}");
    let task_id = task["id"].as_str().unwrap().to_owned();
    let branch = format!(
        "sun/{}-ship-api-web-and-docs",
        task["key"].as_str().unwrap()
    );
    let session_dir = host_dir.join("work").join(&task_id);

    // Wait for the merge_order gate to wait on the first PR.
    let view = |app: Router, task_id: String| async move {
        call(&app, "GET", &format!("/api/v1/tasks/{task_id}"), None)
            .await
            .1
    };
    let events = |app: Router, run: String| async move {
        call(
            &app,
            "GET",
            &format!("/api/v1/runs/{run}/events?after=0"),
            None,
        )
        .await
        .1
    };
    let waiting_on = |events: &Value, slug: &str| {
        events.as_array().unwrap().iter().any(|e| {
            e["kind"] == "gate_result"
                && e["payload"]["result"]["waiting"] == true
                && e["payload"]["result"]["output"]
                    .as_str()
                    .is_some_and(|o| o.contains(&format!("merge next: {slug}#")))
        })
    };
    for _ in 0..1500 {
        let detail = view(app.clone(), task_id.clone()).await;
        let run = detail["run"]["id"].as_str().unwrap_or_default().to_owned();
        if detail["status"] == "failed"
            || (!run.is_empty() && waiting_on(&events(app.clone(), run).await, &slugs[0]))
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    // The run moved on between the view and its events: read it again.
    let mut detail = view(app.clone(), task_id.clone()).await;
    let run = detail["run"]["id"].as_str().unwrap().to_owned();
    assert!(
        waiting_on(&events(app.clone(), run.clone()).await, &slugs[0]),
        "{detail:#}\n{:#}",
        events(app.clone(), run.clone()).await
    );
    assert_eq!(detail["status"], "verifying", "{detail:#}");
    // The run works in the three repos, in list order.
    let run_repos: Vec<String> = detail["run"]["repo_ids"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r.as_str().unwrap().to_owned())
        .collect();
    let listed = call(
        app,
        "GET",
        &format!("/api/v1/workspaces/{MAIN}/repos"),
        None,
    )
    .await
    .1;
    let listed: Vec<String> = listed
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["id"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(run_repos, listed, "{detail:#}");

    // Three worktrees in one session dir, each on the task branch.
    for name in ["api", "web", "docs"] {
        assert!(session_dir.join(name).join(".git").exists(), "{name}");
    }
    // One PR per changed repo, in merge order, each body linking the other.
    let mut prs: Vec<ModCall> = mods
        .calls
        .lock()
        .unwrap()
        .iter()
        .filter(|c| c.method == "open_pr")
        .cloned()
        .collect();
    assert_eq!(prs.len(), 2, "{prs:#?}");
    // The outbox may deliver the two rows in either order; the mod numbers
    // PRs in the order it was called, with head sha `abc<n>def`.
    let sha_of = |slug: &str| -> String {
        let calls = mods.calls.lock().unwrap();
        let n = calls
            .iter()
            .filter(|c| c.method == "open_pr")
            .position(|c| c.params["repo"] == slug)
            .unwrap();
        format!("abc{}def", n + 1)
    };
    prs.sort_by_key(|pr| slugs.iter().position(|s| *s == pr.params["repo"]));
    for (i, pr) in prs.iter().enumerate() {
        assert_eq!(pr.params["repo"], slugs[i]);
        assert_eq!(pr.params["head"], branch);
        let body = pr.params["body"].as_str().unwrap();
        assert!(
            body.contains(&format!("1. {}", slugs[0]))
                && body.contains(&format!("2. {}", slugs[1])),
            "{body}"
        );
        assert!(body.contains(&format!("{} (this PR)", slugs[i])), "{body}");
        assert!(!body.contains(&slugs[2]), "docs has no PR: {body}");
    }
    // Each PR's head sha is stored on the run; the next PR's merge-order
    // status is success, the later one's pending; once both PRs are open,
    // each gets one comment linking both PR URLs.
    let sha = |i: usize| sha_of(&slugs[i]);
    let calls_of = |method: &str| -> Vec<ModCall> {
        let calls = mods.calls.lock().unwrap();
        calls
            .iter()
            .filter(|c| c.method == method)
            .cloned()
            .collect()
    };
    let status_of = |slug: &str| -> Option<String> {
        calls_of("set_status")
            .iter()
            .rev()
            .find(|c| c.params["repo"] == slug)
            .map(|c| c.params["state"].as_str().unwrap().to_owned())
    };
    for _ in 0..500 {
        // Both rows are enqueued together but delivered in either order.
        if calls_of("comment").len() == 2 && slugs[..2].iter().all(|s| status_of(s).is_some()) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(status_of(&slugs[0]).as_deref(), Some("success"));
    assert_eq!(status_of(&slugs[1]).as_deref(), Some("pending"));
    let statuses = calls_of("set_status");
    assert!(
        statuses
            .iter()
            .all(|c| c.params["context"] == "sun/merge-order"
                && c.params["sha"] == sha(usize::from(c.params["repo"] != slugs[0].as_str())))
    );
    let comments = calls_of("comment");
    assert_eq!(comments.len(), 2, "{comments:#?}");
    for c in &comments {
        let body = c.params["body"].as_str().unwrap();
        assert!(
            body.contains("https://git.test/pr/1") && body.contains("https://git.test/pr/2"),
            "{body}"
        );
        assert!(
            c.idem_key
                .as_ref()
                .is_some_and(|k| k.as_str().contains(":links:"))
        );
    }
    // The changed repos' branches are pushed; docs' is not.
    for (name, pushed) in [("api", true), ("web", true), ("docs", false)] {
        let out = std::process::Command::new("git")
            .args([
                "--git-dir",
                dir.path().join(format!("{name}.git")).to_str().unwrap(),
            ])
            .args([
                "rev-parse",
                "--verify",
                "--quiet",
                &format!("refs/heads/{branch}"),
            ])
            .output()
            .unwrap();
        assert_eq!(out.status.success(), pushed, "{name}");
    }

    // Merges in order: api, then web.
    let merge = |slug: &str, number: u64| sun_core::ModEvent {
        mod_name: "github".into(),
        topic: "pr_merged".into(),
        payload: json!({
            "repo": slug, "number": number, "branch": branch.clone(),
            "base": "main", "merge_sha": "abcdef1", "merged_at": "2026-10-07T00:00:00Z",
        }),
    };
    merges.send(merge(&slugs[0], 1)).unwrap();
    // The merge lets the second PR through; the check it started waits in
    // `ci_status` (held) while the second merge lands, then checks again.
    for _ in 0..500 {
        if status_of(&slugs[1]).as_deref() == Some("success") {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(status_of(&slugs[1]).as_deref(), Some("success"));
    merges.send(merge(&slugs[1], 2)).unwrap();
    let merged = |events: &Value| {
        let all = events.as_array().unwrap();
        all.iter().filter(|e| e["kind"] == "pr_merged").count()
    };
    for _ in 0..500 {
        if merged(&events(app.clone(), run.clone()).await) == 2 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    mods.hold.add_permits(tokio::sync::Semaphore::MAX_PERMITS);
    for _ in 0..500 {
        detail = view(app.clone(), task_id.clone()).await;
        // The task is `in_progress` again for a moment after the gate passed.
        if !matches!(detail["status"].as_str(), Some("verifying" | "in_progress")) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(detail["status"], "needs_you", "{detail:#}");
    assert_eq!(detail["run"]["status"], "done", "{detail:#}");
    let events = events(app.clone(), run.clone()).await;
    assert!(waiting_on(&events, &slugs[1]), "{events:#}");
    until_gone(&session_dir).await;
}

/// Records `webhook` calls; refuses (`FAILED` → `InvalidInput`) a body
/// containing `forged`, like mod-slack on a bad signature.
#[derive(Default)]
struct Hooks(std::sync::Mutex<Vec<ModCall>>);

#[async_trait]
impl ModLink for Hooks {
    async fn call(&self, call: ModCall) -> Result<Value> {
        self.0.lock().unwrap().push(call.clone());
        if call.params.to_string().contains("forged") {
            return Err(sun_core::Error::InvalidInput("bad signature".into()));
        }
        Ok(json!({ "response": { "text": "ok" } }))
    }
}

async fn deliver(app: &Router, path: &str, headers: &[(&str, &str)], body: &str) -> StatusCode {
    let mut request = Request::post(path);
    for (name, value) in headers {
        request = request.header(*name, *value);
    }
    let request = request.body(Body::from(body.to_owned())).unwrap();
    app.clone().oneshot(request).await.unwrap().status()
}

#[tokio::test]
async fn webhooks_are_verified_then_forwarded() {
    const TRELLO_URL: &str = "https://sun.example.com/api/v1/webhooks/trello";
    let hooks = Arc::new(Hooks::default());
    let k = kernel_with_mods(hooks.clone()).await;
    let app = &k.app;
    let mut tx = k.store.begin().await.unwrap();
    for name in ["github", "gitea", "gitlab", "jira", "trello"] {
        let scope = sun_core::model::SecretScope::Mod(name.into());
        tx.set_secret(&scope, "WEBHOOK_SECRET", "It's a Secret to Everybody")
            .await
            .unwrap();
    }
    let trello = sun_core::model::SecretScope::Mod("trello".into());
    tx.set_secret(&trello, "WEBHOOK_URL", TRELLO_URL)
        .await
        .unwrap();
    tx.commit().await.unwrap();

    // GitHub's documented example: secret, payload and signature.
    let signature = "sha256=757107ea0eb2509fc211221cce984b8a37570b6d7586c22c46f4379c8b043e17";
    let github = [
        ("x-hub-signature-256", signature),
        ("x-github-event", "ping"),
    ];
    let path = "/api/v1/webhooks/github";
    assert_eq!(
        deliver(app, path, &github, "Hello, World!").await,
        StatusCode::UNPROCESSABLE_ENTITY,
        "not JSON"
    );
    let bad = [
        ("x-hub-signature-256", "sha256=00"),
        ("x-github-event", "ping"),
    ];
    assert_eq!(
        deliver(app, path, &bad, "{}").await,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        deliver(app, path, &[], "{}").await,
        StatusCode::UNAUTHORIZED,
        "unsigned"
    );
    assert!(
        hooks.0.lock().unwrap().is_empty(),
        "nothing unverified reaches a mod"
    );

    // A JSON body signed with the secret goes through as {event, payload}.
    let body = r#"{"zen":"hi"}"#;
    let mac = hex(&hmac::<Sha256>(&[body.as_bytes()]));
    let header = format!("sha256={mac}");
    let ok = [
        ("x-hub-signature-256", header.as_str()),
        ("x-github-event", "issues"),
    ];
    assert_eq!(deliver(app, path, &ok, body).await, StatusCode::OK);

    // GitLab: the token header must equal the secret.
    let gitlab = "/api/v1/webhooks/gitlab";
    let wrong = [("x-gitlab-token", "nope")];
    assert_eq!(
        deliver(app, gitlab, &wrong, "{}").await,
        StatusCode::UNAUTHORIZED
    );
    let right = [
        ("x-gitlab-token", "It's a Secret to Everybody"),
        ("x-gitlab-event", "Merge Request Hook"),
    ];
    assert_eq!(
        deliver(app, gitlab, &right, r#"{"object_kind":"merge_request"}"#).await,
        StatusCode::OK
    );

    // Gitea signs the body (hex, no prefix) and also sends GitHub's header.
    let gitea = "/api/v1/webhooks/gitea";
    let pr = r#"{"number":1}"#;
    let signed = hex(&hmac::<Sha256>(&[pr.as_bytes()]));
    let headers = [
        ("x-gitea-signature", signed.as_str()),
        ("x-hub-signature-256", "sha256=ignored"),
        ("x-gitea-event", "pull_request"),
    ];
    assert_eq!(deliver(app, gitea, &headers, pr).await, StatusCode::OK);
    let forged = [("x-gitea-signature", "00"), ("x-gitea-event", "push")];
    assert_eq!(
        deliver(app, gitea, &forged, pr).await,
        StatusCode::UNAUTHORIZED
    );

    // Jira: `X-Hub-Signature` (sha256=) or `?secret=` in the URL; `{payload}`.
    let issue = r#"{"webhookEvent":"jira:issue_updated"}"#;
    let signed = format!("sha256={}", hex(&hmac::<Sha256>(&[issue.as_bytes()])));
    let jira = "/api/v1/webhooks/jira";
    let headers = [("x-hub-signature", signed.as_str())];
    assert_eq!(deliver(app, jira, &headers, issue).await, StatusCode::OK);
    let by_url = "/api/v1/webhooks/jira?secret=It%27s%20a%20Secret%20to%20Everybody";
    assert_eq!(deliver(app, by_url, &[], issue).await, StatusCode::OK);
    let wrong_url = "/api/v1/webhooks/jira?secret=nope";
    assert_eq!(
        deliver(app, wrong_url, &[], issue).await,
        StatusCode::UNAUTHORIZED
    );

    // Trello: base64 HMAC-SHA1 of body + callback URL; HEAD probe is 200.
    let trello = "/api/v1/webhooks/trello";
    let action = r#"{"action":{"type":"updateCard"}}"#;
    let mac = hmac::<sha1::Sha1>(&[action.as_bytes(), TRELLO_URL.as_bytes()]);
    let signed = base64::engine::general_purpose::STANDARD.encode(mac);
    let headers = [("x-trello-webhook", signed.as_str())];
    assert_eq!(deliver(app, trello, &headers, action).await, StatusCode::OK);
    let unsigned_url = hmac::<sha1::Sha1>(&[action.as_bytes()]);
    let wrong = base64::engine::general_purpose::STANDARD.encode(unsigned_url);
    let headers = [("x-trello-webhook", wrong.as_str())];
    assert_eq!(
        deliver(app, trello, &headers, action).await,
        StatusCode::UNAUTHORIZED,
        "the callback URL is part of the signature"
    );
    let probe = Request::head(trello).body(Body::empty()).unwrap();
    let response = app.clone().oneshot(probe).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    // Slack: no kernel secret, raw request forwarded; the mod's refusal is a 401.
    let slack = "/api/v1/webhooks/slack";
    let signed = [
        ("x-slack-signature", "v0=abc"),
        ("authorization", "Bearer x"),
    ];
    assert_eq!(
        deliver(app, slack, &signed, "token=forged").await,
        StatusCode::UNAUTHORIZED
    );
    let request = Request::post(slack)
        .header("x-slack-signature", "v0=abc")
        .body(Body::from("command=%2Fsun"))
        .unwrap();
    let response = app.clone().oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    assert_eq!(
        serde_json::from_slice::<Value>(&bytes).unwrap(),
        json!({ "text": "ok" })
    );

    let calls = hooks.0.lock().unwrap().clone();
    let shapes: Vec<_> = calls
        .iter()
        .map(|c| (c.mod_name.as_str(), c.method.as_str()))
        .collect();
    assert_eq!(
        shapes,
        [
            ("github", "webhook"),
            ("gitlab", "webhook"),
            ("gitea", "webhook"),
            ("jira", "webhook"),
            ("jira", "webhook"),
            ("trello", "webhook"),
            ("slack", "webhook"),
            ("slack", "webhook")
        ]
    );
    assert_eq!(
        calls[0].params,
        json!({ "event": "issues", "payload": { "zen": "hi" } })
    );
    assert_eq!(calls[1].params["event"], "Merge Request Hook");
    assert_eq!(
        calls[2].params,
        json!({ "event": "pull_request", "payload": { "number": 1 } })
    );
    assert_eq!(
        calls[3].params,
        json!({ "payload": { "webhookEvent": "jira:issue_updated" } })
    );
    assert_eq!(calls[5].params["payload"]["action"]["type"], "updateCard");
    assert_eq!(calls[6].params["body"], "token=forged");
    assert_eq!(calls[6].params["headers"]["x-slack-signature"], "v0=abc");
    assert!(calls[6].params["headers"].get("authorization").is_none());
}

/// HMAC with the test's `WEBHOOK_SECRET` (64-byte-block hashes), the
/// independent check of the kernel's own.
fn hmac<D: Digest>(parts: &[&[u8]]) -> Vec<u8> {
    let key = b"It's a Secret to Everybody";
    let mut block = [0u8; 64];
    block[..key.len()].copy_from_slice(key);
    let mut inner = D::new().chain_update(block.map(|b| b ^ 0x36));
    for part in parts {
        inner.update(part);
    }
    D::new()
        .chain_update(block.map(|b| b ^ 0x5c))
        .chain_update(inner.finalize())
        .finalize()
        .to_vec()
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[tokio::test]
async fn knowledge_flags_route_reports_stale_and_conflicting_sources() {
    use sun_context::knowledge::{DocContent, SourceDoc, add_source, ingest};
    let kernel = kernel().await;
    let source = add_source(&kernel.store, "upload", "docs", json!({}))
        .await
        .unwrap();
    for (id, title, days, price) in [
        ("prd.md", "PRD", 400, "99.000"),
        ("deck.md", "Deck", 3, "120.000"),
    ] {
        let doc = SourceDoc {
            external_id: id.into(),
            title: title.into(),
            version: "v1".into(),
            updated_at: Some(chrono::Utc::now() - chrono::Duration::days(days)),
            url: None,
            acl: Vec::new(),
            content: DocContent::Markdown(format!(
                "# {title}\n\n## Pricing\n\nThe plan costs IDR {price} per month.\n"
            )),
        };
        ingest(&kernel.store, &source, doc).await.unwrap();
    }
    let other = new_task(&kernel.app, APPROVAL).await;
    let (status, task) = call(
        &kernel.app,
        "POST",
        "/api/v1/tasks",
        Some(json!({ "title": "Change the plan pricing", "workflow_id": other["workflow"]["id"] })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{task}");
    let path = format!(
        "/api/v1/tasks/{}/knowledge_flags",
        task["id"].as_str().unwrap()
    );
    let (status, flags) = call(&kernel.app, "GET", &path, None).await;
    assert_eq!(status, StatusCode::OK, "{flags}");
    let kinds: Vec<&str> = flags
        .as_array()
        .unwrap()
        .iter()
        .map(|f| f["kind"].as_str().unwrap())
        .collect();
    assert_eq!(kinds, ["stale", "conflict"], "{flags}");
    assert_eq!(flags[1]["citations"].as_array().unwrap().len(), 2);
    assert!(
        flags[0]["pointers"][0]
            .as_str()
            .unwrap()
            .starts_with("doc:")
    );
    let (status, _) = call(
        &kernel.app,
        "GET",
        "/api/v1/tasks/nope/knowledge_flags",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
}

#[tokio::test]
async fn decisions_export_and_replay_routes() {
    let k = kernel().await;
    let (status, body) = call(&k.app, "GET", "/api/v1/decisions/export", None).await;
    assert_eq!((status, body), (StatusCode::OK, Value::Null), "nothing yet");
    let bad = json!({ "url": "ftp://laya" });
    let (status, _) = call(&k.app, "POST", "/api/v1/decisions/replay", Some(bad)).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    // No past decisions: nothing is asked and nothing switches.
    let idle = json!({ "url": "http://127.0.0.1:1" });
    let (status, body) = call(&k.app, "POST", "/api/v1/decisions/replay", Some(idle)).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["switched"], false);
    assert_eq!(body["candidate"]["asked"], 0);
}

#[tokio::test]
async fn corrections_from_rejects_and_pushed_reverts_are_listed() {
    let hooks = Arc::new(Hooks::default());
    let k = kernel_with_mods(hooks).await;
    let app = &k.app;
    let mut tx = k.store.begin().await.unwrap();
    let scope = sun_core::model::SecretScope::Mod("gitlab".into());
    tx.set_secret(&scope, "WEBHOOK_SECRET", "s3cret-hook")
        .await
        .unwrap();
    tx.commit().await.unwrap();

    let task = new_task(app, APPROVAL).await;
    let id = task["id"].as_str().unwrap();
    until(app, id, "needs_you").await;
    // Not one answer, then not a decide step: refused, nothing recorded.
    let path = format!("/api/v1/tasks/{id}/decision");
    let (status, _) = call(app, "POST", &path, Some(json!({ "answer": [1] }))).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    let answer = json!({ "answer": { "choice": "billing" } });
    let (status, _) = call(app, "POST", &path, Some(answer)).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    let reject = format!("/api/v1/tasks/{id}/reject");
    let reason = json!({ "reason": "Tests fail on the empty cart" });
    let (status, body) = call(app, "POST", &reject, Some(reason)).await;
    assert_eq!(status, StatusCode::OK, "{body}");

    // A GitLab push to the task branch (`sun/<KEY>-<slug>`) that reverts a
    // commit, delivered twice.
    let key = task["key"].as_str().unwrap();
    let push = json!({ "ref": format!("refs/heads/sun/{key}-add-cart"), "commits": [
        { "id": "a1b2c3d4e5", "message": "feat: add cart" },
        { "id": "f6e5d4c3b2", "message": "Revert \"feat: add cart\"\n\nThis reverts commit a1b2c3d4e5." },
    ]})
    .to_string();
    let headers = [
        ("x-gitlab-token", "s3cret-hook"),
        ("x-gitlab-event", "Push Hook"),
    ];
    for _ in 0..2 {
        let status = deliver(app, "/api/v1/webhooks/gitlab", &headers, &push).await;
        assert_eq!(status, StatusCode::OK);
    }

    let path = format!("/api/v1/tasks/{id}/corrections");
    let (status, rows) = call(app, "GET", &path, None).await;
    assert_eq!(status, StatusCode::OK, "{rows}");
    let got: Vec<(&str, &str)> = rows
        .as_array()
        .unwrap()
        .iter()
        .map(|c| {
            (
                c["kind"].as_str().unwrap(),
                c["cause_label"].as_str().unwrap(),
            )
        })
        .collect();
    let expected = [("reject", "test_failure"), ("revert", "other")];
    assert_eq!(got, expected, "{rows:#}");
    assert_eq!(rows[0]["body"], "Tests fail on the empty cart");
    assert_eq!(rows[0]["step_key"], "approve");
    assert_eq!(rows[1]["step_key"], Value::Null);
    let body = rows[1]["body"].as_str().unwrap();
    assert!(
        body.starts_with("Commit f6e5d4c3b2 reverted: Revert"),
        "{body}"
    );
    let (_, all) = call(app, "GET", "/api/v1/corrections?limit=1", None).await;
    assert_eq!(all.as_array().unwrap().len(), 1);
    assert_eq!(all[0]["kind"], "revert", "newest first");
    let (status, _) = call(app, "GET", "/api/v1/tasks/nope/corrections", None).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
}

#[tokio::test]
async fn memory_star_review_and_metrics_routes() {
    let kernel = kernel().await;
    let app = &kernel.app;
    let task = new_task(app, APPROVAL).await;
    let id = task["id"].as_str().unwrap();
    until(app, id, "needs_you").await;
    let reason = json!({ "reason": "Wrong scope: don't touch the billing module" });
    let (status, _) = call(
        app,
        "POST",
        &format!("/api/v1/tasks/{id}/reject"),
        Some(reason),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (_, rows) = call(app, "GET", &format!("/api/v1/tasks/{id}/corrections"), None).await;
    let star = format!(
        "/api/v1/corrections/{}/star",
        rows[0]["id"].as_str().unwrap()
    );
    let (status, body) = call(app, "POST", &star, Some(json!({}))).await;
    assert_eq!(
        (status, &body["created"]),
        (StatusCode::OK, &json!(true)),
        "{body}"
    );
    let (_, body) = call(app, "POST", &star, Some(json!({}))).await;
    assert_eq!(body["created"], json!(false), "a second star is a no-op");

    let (status, items) = call(app, "GET", "/api/v1/memory", None).await;
    assert_eq!(status, StatusCode::OK, "{items}");
    let item = &items[0];
    assert_eq!(
        (&item["status"], &item["scope"]),
        (&json!("proposed"), &json!("workflow"))
    );
    assert_eq!(item["pr"], Value::Null, "no repo, no PR: an approval card");
    assert!(item["author"].as_str().unwrap().starts_with("user:"));
    let review = format!("/api/v1/memory/{}/review", item["id"].as_str().unwrap());
    let (status, _) = call(app, "POST", &review, Some(json!({ "status": "proposed" }))).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    let (status, body) = call(app, "POST", &review, Some(json!({ "status": "active" }))).await;
    assert_eq!(
        (status, &body["status"]),
        (StatusCode::OK, &json!("active")),
        "{body}"
    );
    let (status, _) = call(app, "POST", &review, Some(json!({ "status": "active" }))).await;
    assert_eq!(status, StatusCode::CONFLICT);

    let (status, m) = call(app, "GET", "/api/v1/memory/metrics", None).await;
    assert_eq!(status, StatusCode::OK, "{m}");
    assert_eq!(
        (&m["overall"]["corrections"], &m["overall"]["repeats"]),
        (&json!(1), &json!(0))
    );
    assert_eq!(
        (&m["active_items"], &m["proposed_items"]),
        (&json!(1), &json!(0))
    );
    assert_eq!(m["weeks"].as_array().unwrap().len(), 8);
    let (status, _) = call(app, "POST", "/api/v1/notes/nope/star", Some(json!({}))).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
}

#[tokio::test]
async fn data_touched_and_quality_routes_before_a_merge() {
    let kernel = kernel().await;
    let task = new_task(&kernel.app, APPROVAL).await;
    let path = format!("/api/v1/tasks/{}/quality", task["id"].as_str().unwrap());
    let (status, view) = call(&kernel.app, "GET", &path, None).await;
    assert_eq!(status, StatusCode::OK, "{view}");
    assert_eq!(
        view,
        json!({ "data_touched": [], "quality": null, "held_up": null })
    );
    let (status, err) = call(&kernel.app, "POST", &format!("{path}/check"), None).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{err}");
    let (status, metrics) = call(&kernel.app, "GET", "/api/v1/quality/metrics", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(metrics["merged"], 0);
    assert_eq!(metrics["held_up_rate"], Value::Null);
}

#[tokio::test]
async fn eval_routes_replay_a_rejected_task() {
    let kernel = kernel().await;
    let app = &kernel.app;
    let (status, body) = call(app, "POST", "/api/v1/evals", Some(json!({"name": "x"}))).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    let by_scope = json!({"name": "scope rule", "pick": {"by": "cause", "cause": "wrong_scope"}});
    let (status, body) = call(app, "POST", "/api/v1/evals", Some(by_scope.clone())).await;
    assert_eq!(
        status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "no task yet: {body}"
    );

    let task = new_task(app, APPROVAL).await;
    let id = task["id"].as_str().unwrap();
    until(app, id, "needs_you").await;
    let reason = json!({ "reason": "Wrong scope: don't touch the billing module" });
    let reject = format!("/api/v1/tasks/{id}/reject");
    let (status, _) = call(app, "POST", &reject, Some(reason)).await;
    assert_eq!(status, StatusCode::OK);

    let set = json!({"name": "billing", "task_ids": [id]});
    let (status, set) = call(app, "POST", "/api/v1/eval_sets", Some(set)).await;
    assert_eq!(status, StatusCode::CREATED, "{set}");
    let (_, sets) = call(app, "GET", "/api/v1/eval_sets", None).await;
    assert_eq!(sets[0]["task_ids"], json!([id]));

    let eval = async |spec: Value| {
        let (status, eval) = call(app, "POST", "/api/v1/evals", Some(spec)).await;
        assert_eq!(status, StatusCode::ACCEPTED, "{eval}");
        assert_eq!(eval["status"], "running");
        let path = format!("/api/v1/evals/{}", eval["id"].as_str().unwrap());
        let mut done = Value::Null;
        for _ in 0..200 {
            (_, done) = call(app, "GET", &path, None).await;
            if done["status"] != "running" {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert_eq!(done["status"], "done", "{done}");
        done["report"].clone()
    };
    // The recorded human rejection recurs, with its reason's cause.
    let report = eval(by_scope.clone()).await;
    let causes = &report["tasks"][0]["original"]["causes"];
    assert_eq!(causes, &json!(["wrong_scope"]));
    assert_eq!(report["tasks"][0]["replay"]["status"], "failed");
    assert_eq!(report["tasks"][0]["replay"]["causes"], *causes);
    assert_eq!(report["pass_delta"], 0.0);
    // Auto-approve, asked for: the rejected run passes in the sandbox.
    let mut auto = by_scope;
    auto["auto_approve"] = json!(true);
    let report = eval(auto).await;
    assert_eq!(report["tasks"][0]["replay"]["status"], "succeeded");
    assert_eq!(report["pass_delta"], 1.0);
    let (_, evals) = call(app, "GET", "/api/v1/evals", None).await;
    assert_eq!(evals.as_array().unwrap().len(), 2);
}

#[tokio::test]
async fn device_screenshots_are_served_only_for_their_run() {
    let k = kernel().await;
    let app = &k.app;
    let task = new_task(app, APPROVAL).await;
    let view = until(app, task["id"].as_str().unwrap(), "needs_you").await;
    let run: RunId = view["run"]["id"].as_str().unwrap().parse().unwrap();
    let shot = k.store.blobs().put(b"PNGshot".to_vec()).await.unwrap();
    let other = k.store.blobs().put(b"PNGother".to_vec()).await.unwrap();
    let mut tx = k.store.begin().await.unwrap();
    sun_store::EventLog::append_event(
        &mut tx,
        sun_store::NewEvent::new(
            run,
            Some(sun_core::StepKey::new("approve").unwrap()),
            "device_result",
            json!({ "device": "Pixel 8", "passed": true, "output": "",
                    "screenshots": [{ "name": "sun-final.png", "blob": shot.as_str() }] }),
        ),
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();

    let get = |path: String| async move {
        let request = Request::builder()
            .uri(path)
            .header(header::AUTHORIZATION, format!("Bearer {TOKEN}"))
            .body(Body::empty())
            .unwrap();
        let response = app.clone().oneshot(request).await.unwrap();
        let status = response.status();
        let kind = response.headers().get(header::CONTENT_TYPE).cloned();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        (status, kind, bytes)
    };
    let path = |run: RunId, blob: &str| format!("/api/v1/runs/{run}/screenshots/{blob}");
    let (status, kind, bytes) = get(path(run, shot.as_str())).await;
    assert_eq!((status, &bytes[..]), (StatusCode::OK, &b"PNGshot"[..]));
    assert_eq!(kind.unwrap(), "image/png");
    // A stored blob the run doesn't cite, another run, a bad hash: refused.
    let (status, ..) = get(path(run, other.as_str())).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, ..) = get(path(RunId::generate(), shot.as_str())).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, ..) = get(path(run, "not-a-hash")).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
}

/// Serves `k`, connects a scripted host (`hello`, with `capabilities`
/// fields merged in) and waits until it is online.
async fn scripted_host(k: &common::Kernel, capabilities: Value) -> Socket {
    let mut hello = kernel_host_hello(k);
    for (field, value) in capabilities.as_object().unwrap() {
        hello["capabilities"][field] = value.clone();
    }
    connect_host(&k.app, k.host, hello).await
}

/// A protocol 2.0 `hello` of `k`'s own host, with its real key.
fn kernel_host_hello(k: &common::Kernel) -> Value {
    let key: String = Sha256::digest(format!("{SECRET}:{}", k.host).as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    hello(k.host, &key)
}

/// Serves `app`, connects as `host` with `hello` and waits until it is online.
async fn connect_host(app: &Router, host: HostId, hello: Value) -> Socket {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("ws://{}/host", listener.local_addr().unwrap());
    tokio::spawn(axum::serve(listener, app.clone()).into_future());
    let (mut socket, _) = tokio_tungstenite::connect_async(&url).await.unwrap();
    send(&mut socket, hello).await;
    until_online(app, host).await;
    socket
}

/// The `fake` runtime's state in `GET /runtimes`.
async fn fake_runtime_status(app: &Router) -> Value {
    let (_, runtimes) = call(app, "GET", "/api/v1/runtimes", None).await;
    let fake = runtimes
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["tool"] == "fake");
    fake.unwrap()["state"].clone()
}

/// The `fake` runtime of a `hello`/`capabilities` report.
fn fake_runtime(logged_in: bool) -> Value {
    json!([{ "adapter": "fake", "effort_options": [], "models": [],
        "readiness": { "installed": true, "version": "1", "logged_in": logged_in,
            "detail": if logged_in { Value::Null } else { json!("Fake is not logged in. Run: fake login") } } }])
}

#[tokio::test]
async fn a_login_after_hello_shows_after_one_reprobe() {
    let k = kernel().await;
    let app = &k.app;
    let mut hello = kernel_host_hello(&k);
    hello["protocol"] = json!("2.1");
    hello["capabilities"]["runtimes"] = fake_runtime(false);
    let mut host = connect_host(app, k.host, hello.clone()).await;
    assert_eq!(fake_runtime_status(app).await, "needs_login");

    // `fake login` on the machine; "Check again" asks the host to re-probe.
    let main = "00000000000000000000000001";
    let path = format!("/api/v1/workspaces/{main}/hosts/{}/reprobe", k.host);
    let check = tokio::spawn({
        let app = app.clone();
        async move { call(&app, "POST", &path, None).await }
    });
    let frame = recv(&mut host).await.unwrap();
    assert_eq!(frame["type"], "reprobe");
    let mut capabilities = hello["capabilities"].clone();
    capabilities["runtimes"] = fake_runtime(true);
    send(
        &mut host,
        json!({ "type": "capabilities", "capabilities": capabilities }),
    )
    .await;
    send(
        &mut host,
        json!({ "type": "reply", "id": frame["id"], "body": { "kind": "done" } }),
    )
    .await;
    let (status, _) = check.await.unwrap();
    assert_eq!(status, StatusCode::NO_CONTENT);
    // Stored before the 204: no polling needed.
    assert_eq!(fake_runtime_status(app).await, "idle");

    // A logout seen by the 5-min re-probe arrives unasked.
    capabilities["runtimes"] = fake_runtime(false);
    send(
        &mut host,
        json!({ "type": "capabilities", "capabilities": capabilities }),
    )
    .await;
    let mut status = Value::Null;
    for _ in 0..200 {
        status = fake_runtime_status(app).await;
        if status == "needs_login" {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(status, "needs_login");

    // Not attached to that workspace (or no such host): 404, nothing sent.
    let elsewhere = format!(
        "/api/v1/workspaces/{main}/hosts/{}/reprobe",
        HostId::generate()
    );
    assert_eq!(
        call(app, "POST", &elsewhere, None).await.0,
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn a_2_0_host_without_effort_or_reprobe_still_connects() {
    let k = kernel().await;
    let app = &k.app;
    // `hello` as 2.0 hosts send it: no `effort_options` or `models`.
    let _host = scripted_host(&k, json!({})).await;
    assert_eq!(fake_runtime_status(app).await, "idle");
    let main = "00000000000000000000000001";
    let path = format!("/api/v1/workspaces/{main}/hosts/{}/reprobe", k.host);
    // It wouldn't understand `reprobe`: refused at once, not a 30 s wait.
    let (status, body) = call(app, "POST", &path, None).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{body}");
    assert!(body.to_string().contains("too old"), "{body}");
}

#[tokio::test]
async fn a_host_that_stops_answering_pings_goes_offline() {
    let k = kernel().await;
    let app = sun_api::router(
        k.api
            .clone()
            .with_host_liveness(Duration::from_millis(50), Duration::from_millis(300)),
    );
    let mut host = connect_host(&app, k.host, kernel_host_hello(&k)).await;
    let status = |app: Router, host: HostId| async move {
        let (_, hosts) = call(&app, "GET", "/api/v1/hosts", None).await;
        let row = hosts
            .as_array()
            .unwrap()
            .iter()
            .find(|h| h["id"] == json!(host))
            .cloned();
        row.unwrap()["status"].clone()
    };

    // Polled, the socket answers pings: online well past the 300 ms limit.
    let drained = tokio::time::timeout(Duration::from_millis(900), async {
        while let Some(Ok(_)) = host.next().await {}
    })
    .await;
    assert!(drained.is_err(), "the kernel kept the connection");
    assert_eq!(status(app.clone(), k.host).await, "online");

    // Then a half-open socket: still connected, never read, no pongs.
    let started = std::time::Instant::now();
    let mut presence = Value::Null;
    while started.elapsed() < Duration::from_secs(60) {
        presence = status(app.clone(), k.host).await;
        if presence == "offline" {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(presence, "offline", "after {:?}", started.elapsed());
    drop(host);
}

/// Plays the host's side of starting an agent session in `path`; the
/// session id.
async fn host_starts_session(host: &mut Socket, path: &str) -> Value {
    let prepare = recv(host).await.unwrap();
    assert_eq!(prepare["type"], "worktree.prepare");
    let body = json!({ "kind": "worktree", "path": path });
    send(
        host,
        json!({ "type": "reply", "id": prepare["id"], "body": body }),
    )
    .await;
    let write = recv(host).await.unwrap();
    assert_eq!(write["type"], "worktree.write");
    let body = json!({ "kind": "done" });
    send(
        host,
        json!({ "type": "reply", "id": write["id"], "body": body }),
    )
    .await;
    let start = recv(host).await.unwrap();
    assert_eq!(start["type"], "session.start");
    let session_id = start["spec"]["session_id"].clone();
    let handle = json!({ "session_id": session_id, "native_id": null, "pid": 7 });
    let body = json!({ "kind": "session", "handle": handle });
    send(
        host,
        json!({ "type": "reply", "id": start["id"], "body": body }),
    )
    .await;
    session_id
}

/// Expects a `session.stop {how: kill}` and acknowledges it.
async fn host_stops_session(host: &mut Socket) {
    let stop = recv(host).await.unwrap();
    assert_eq!(stop["type"], "session.stop", "{stop}");
    assert_eq!(stop["how"], "kill");
    let body = json!({ "kind": "done" });
    send(
        host,
        json!({ "type": "reply", "id": stop["id"], "body": body }),
    )
    .await;
}

#[tokio::test]
async fn hosts_expose_session_stats_devices_and_kill() {
    let k = kernel().await;
    let app = &k.app;
    let tool = |ready: bool, devices: Value, detail: Value| json!({ "ready": ready, "version": null, "devices": devices, "detail": detail });
    let devices = json!({ "devices": {
        "ios_simulator": tool(true, json!(["iPhone 15 (iOS 17.0)"]), Value::Null),
        "android_emulator": tool(false, json!([]), json!("install a system image with sdkmanager")),
        "maestro": tool(true, json!([]), Value::Null),
    } });
    let mut host = scripted_host(&k, devices).await;
    let task = new_task(app, AGENT).await;
    let task_id = task["id"].as_str().unwrap();
    let session_id = host_starts_session(&mut host, "/data/work/t").await;
    let view = until(app, task_id, "in_progress").await;
    let run_id = view["run"]["id"].as_str().unwrap().to_owned();

    // The host's 5 s stats and its per-task disk use reach `GET /hosts`.
    let mib = 1024 * 1024;
    let stats = json!({ "type": "stats", "session_id": session_id,
        "rss_bytes": 300 * mib, "cpu_percent": 12.5, "uptime_s": 42 });
    send(&mut host, stats).await;
    send(
        &mut host,
        json!({ "type": "disk", "tasks": [[task_id, 5 * mib]] }),
    )
    .await;
    let mut hosts = Value::Null;
    for _ in 0..200 {
        (_, hosts) = call(app, "GET", "/api/v1/hosts", None).await;
        if hosts[0]["sessions"] == 1 && hosts[0]["disk"][0]["bytes"].is_u64() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let h = &hosts[0];
    let session = &h["session_stats"][0];
    assert_eq!(session["session_id"], session_id, "{h}");
    assert_eq!(session["rss_bytes"], 300 * mib);
    assert_eq!(session["cpu_percent"], 12.5);
    assert_eq!(session["uptime_s"], 42);
    assert_eq!(session["run_id"], run_id.as_str());
    assert_eq!(session["step_key"], "work");
    assert_eq!(h["disk"], json!([{ "task_id": task_id, "bytes": 5 * mib }]));
    // Detected devices, and the fix for what's missing.
    let caps = &h["capabilities"];
    assert_eq!(caps["devices"], json!(["iPhone 15 (iOS 17.0)"]));
    let android = &caps["device_tools"]["android_emulator"]["detail"];
    assert!(android.as_str().unwrap().contains("system image"), "{h}");

    // The kernel's own resources, apart from the sessions.
    let (status, kernel) = call(app, "GET", "/api/v1/kernel", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(kernel["pid"], std::process::id());
    assert!(kernel["rss_bytes"].as_u64().unwrap() > 0, "{kernel}");

    // An admin kills the session with a reason; the run pauses and says why.
    let kill = format!("/api/v1/sessions/{}/kill", session_id.as_str().unwrap());
    let (status, _) = call(app, "POST", &kill, Some(json!({ "reason": " " }))).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    let why = json!({ "reason": "box is out of RAM" });
    let ((status, answer), ()) = tokio::join!(
        call(app, "POST", &kill, Some(why)),
        host_stops_session(&mut host)
    );
    assert_eq!(status, StatusCode::OK, "{answer}");
    assert_eq!(answer["run_id"], run_id.as_str());
    let view = until(app, task_id, "blocked").await;
    let why = "session killed by user:me: box is out of RAM";
    assert_eq!(view["run"]["end_detail"], why, "{view}");
    assert_eq!(view["run"]["steps"][0]["status"], "interrupted");
    let (_, events) = call(app, "GET", &format!("/api/v1/runs/{run_id}/events"), None).await;
    assert!(events.to_string().contains("session_killed"), "{events}");
}

#[tokio::test]
async fn per_host_cap_queues_the_second_task() {
    let one = sun_engine::HostCap {
        runs: Some(1),
        ..Default::default()
    };
    let caps = sun_engine::Caps {
        per_host: [("box".to_owned(), one)].into(),
        ..Default::default()
    };
    let k = common::kernel_with_caps(caps).await;
    let app = &k.app;
    let mut host = scripted_host(&k, json!({})).await;
    let first = new_task(app, AGENT).await;
    let first_id = first["id"].as_str().unwrap();
    host_starts_session(&mut host, "/data/work/one").await;
    until(app, first_id, "in_progress").await;
    let second = new_task(app, AGENT).await;
    // The host is full: nothing is prepared for the second task.
    let next = tokio::time::timeout(Duration::from_millis(500), host.next()).await;
    assert!(next.is_err(), "the host got a second run: {next:?}");
    let second_id = second["id"].as_str().unwrap();
    let (_, view) = call(app, "GET", &format!("/api/v1/tasks/{second_id}"), None).await;
    assert_eq!(view["status"], "todo", "{view}");
    // Cancelling the first frees the slot: the second is prepared next.
    let cancel = format!("/api/v1/tasks/{first_id}/cancel");
    tokio::join!(
        call(app, "POST", &cancel, None),
        host_stops_session(&mut host)
    );
    for _ in 0..5 {
        let frame = recv(&mut host).await.unwrap();
        if frame["type"] == "worktree.prepare" {
            return;
        }
        let body = json!({ "kind": "done" });
        send(
            &mut host,
            json!({ "type": "reply", "id": frame["id"], "body": body }),
        )
        .await;
    }
    panic!("the second task never started");
}

/// A host's `runtimes` report: Claude Code installed, logged in or not, and
/// Codex not installed (no binary: not a runtime).
fn claude_code(logged_in: bool) -> Value {
    let detail = if logged_in {
        Value::Null
    } else {
        json!("Claude Code is not logged in. Run: claude auth login")
    };
    json!([
        { "adapter": "claude-code", "effort_options": [], "models": [], "readiness":
            { "installed": true, "version": "2.1.291", "logged_in": logged_in, "detail": detail } },
        { "adapter": "codex", "readiness":
            { "installed": false, "version": null, "logged_in": false, "detail": "Codex not found" } }
    ])
}

/// Enrols a host named `name` into Main and connects it with `runtimes`.
async fn enrol_host(app: &Router, name: &str, runtimes: Value) -> (HostId, Socket) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("ws://{}/host", listener.local_addr().unwrap());
    tokio::spawn(axum::serve(listener, app.clone()).into_future());
    let tokens = "/api/v1/workspaces/00000000000000000000000001/hosts/tokens";
    let (_, issued) = call(app, "POST", tokens, None).await;
    let enroll = json!({ "type": "enroll", "token": issued["token"], "name": name });
    let enrolled = ask(&url, enroll).await.unwrap();
    let host: HostId = serde_json::from_value(enrolled["host_id"].clone()).unwrap();
    let mut hello = hello(host, enrolled["host_key"].as_str().unwrap());
    hello["capabilities"]["runtimes"] = runtimes;
    (host, connect_host(app, host, hello).await)
}

/// `GET /runtimes` entries of `tool`, keyed by host id.
async fn runtimes_of(app: &Router, tool: &str) -> std::collections::HashMap<String, Value> {
    let (status, runtimes) = call(app, "GET", "/api/v1/runtimes", None).await;
    assert_eq!(status, StatusCode::OK, "{runtimes}");
    runtimes
        .as_array()
        .unwrap()
        .iter()
        .filter(|r| r["tool"] == tool)
        .map(|r| (r["host"]["id"].as_str().unwrap().to_owned(), r.clone()))
        .collect()
}

/// Polls until `host`'s `tool` entry is in `state`.
async fn until_state(app: &Router, tool: &str, host: HostId, state: &str) -> Value {
    for _ in 0..500 {
        if let Some(entry) = runtimes_of(app, tool).await.remove(&host.to_string())
            && entry["state"] == state
        {
            return entry;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("{tool} on {host} never became {state}");
}

#[tokio::test]
async fn runtimes_list_each_host_and_tool_with_an_honest_state() {
    let k = kernel().await;
    let app = &k.app;
    let mut hello = kernel_host_hello(&k);
    hello["capabilities"]["runtimes"] = claude_code(true);
    let laptop = connect_host(app, k.host, hello).await;
    let (vps, vps_socket) = enrol_host(app, "vps-sg-1", claude_code(false)).await;

    // Two hosts with Claude Code are two entries; Codex, not installed, is none.
    let claude = runtimes_of(app, "claude-code").await;
    assert_eq!(claude.len(), 2, "{claude:?}");
    assert!(runtimes_of(app, "codex").await.is_empty());
    let ready = &claude[&k.host.to_string()];
    assert_eq!(ready["id"], format!("{}:claude-code", k.host));
    assert_eq!(ready["name"], "claude-code", "what chains name");
    assert_eq!(ready["source"], "byo_runtime");
    assert_eq!(ready["version"], "2.1.291");
    assert_eq!(
        (&ready["presence"], &ready["availability"], &ready["state"]),
        (&json!("online"), &json!("ready"), &json!("idle"))
    );
    assert_eq!(ready["load"], json!({ "active": 0, "max": null }));
    assert_eq!(ready["detail"], Value::Null);

    // Installed but logged out: needs_login, with the command to run there.
    let logged_out = &claude[&vps.to_string()];
    assert_eq!(logged_out["host"]["name"], "vps-sg-1");
    assert_eq!(logged_out["availability"], "needs_login");
    assert_eq!(logged_out["state"], "needs_login");
    assert_eq!(logged_out["detail"], "Run `claude auth login` on vps-sg-1");

    // A cooldown: limited until then, on the host that is logged in; the
    // logged-out one still needs a login first.
    let until = chrono::Utc::now() + chrono::Duration::minutes(30);
    let mut tx = k.store.begin().await.unwrap();
    let cooled = tx
        .upsert_runtime(
            MAIN,
            "claude-code",
            "claude-code",
            RuntimeSource::ByoRuntime,
        )
        .await
        .unwrap();
    tx.set_cooldown(cooled.id, Some(until)).await.unwrap();
    tx.commit().await.unwrap();
    let claude = runtimes_of(app, "claude-code").await;
    let limited = &claude[&k.host.to_string()];
    assert_eq!(limited["state"], "limited");
    let back: chrono::DateTime<chrono::Utc> =
        serde_json::from_value(limited["cooldown_until"].clone()).unwrap();
    assert!((back - until).num_seconds().abs() <= 1, "{back} vs {until}");
    assert!(
        limited["detail"]
            .as_str()
            .unwrap()
            .starts_with("Usage limit reached; back at"),
        "{limited}"
    );
    assert_eq!(claude[&vps.to_string()]["state"], "needs_login");

    // A host that goes away is offline, whatever its login or cooldown.
    drop(vps_socket);
    let gone = until_state(app, "claude-code", vps, "offline").await;
    assert_eq!(gone["presence"], "offline");
    assert_eq!(gone["detail"], "vps-sg-1 is offline");
    drop(laptop);
    until_state(app, "claude-code", k.host, "offline").await;
}

/// `?model=v1` is the old per-adapter list, unchanged: one entry per
/// adapter across hosts, `status` ready/offline.
#[tokio::test]
async fn runtimes_v1_keeps_the_old_per_adapter_list() {
    let k = kernel().await;
    let app = &k.app;
    let mut hello = kernel_host_hello(&k);
    hello["capabilities"]["runtimes"] = claude_code(true);
    let _laptop = connect_host(app, k.host, hello).await;
    let (_vps, _socket) = enrol_host(app, "vps-sg-1", claude_code(false)).await;
    let (status, v1) = call(app, "GET", "/api/v1/runtimes?model=v1", None).await;
    assert_eq!(status, StatusCode::OK, "{v1}");
    let allow_bypass = v1[0]["allow_bypass"].clone();
    assert_eq!(
        v1,
        json!([
            { "model": null, "workspace_id": null, "id": "claude-code", "name": "claude-code",
              "adapter": "claude-code", "source": "byo_runtime", "status": "ready",
              "cooldown_until": null, "sessions": 0, "max_sessions": 0, "permission_mode": null,
              "allow_bypass": allow_bypass, "config": null },
            { "model": null, "workspace_id": null, "id": "codex", "name": "codex",
              "adapter": "codex", "source": "byo_runtime", "status": "offline",
              "cooldown_until": null, "sessions": 0, "max_sessions": 0, "permission_mode": null,
              "allow_bypass": allow_bypass, "config": null },
        ])
    );
    let (status, v2) = call(app, "GET", "/api/v1/runtimes?model=v2", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(v2.as_array().unwrap().len(), 2, "one per host: {v2}");
    let (status, body) = call(app, "GET", "/api/v1/runtimes?model=v3", None).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
}
