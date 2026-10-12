//! Agents over HTTP: CRUD (DELETE archives) behind `manage_runtimes`,
//! readiness, assigning tasks, and the hidden system workflow.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use axum::http::StatusCode;
use serde_json::{Value, json};
use sun_core::GroupId;

use crate::common::kernel;
use crate::workspaces::{FLOW, call, call_as, member_token};

const MAIN: &str = "00000000000000000000000001";

#[tokio::test]
async fn agents_crud_needs_manage_runtimes_and_reads_are_open() {
    let k = kernel().await;
    let app = &k.app;
    for bad in [
        json!({ "runtime_chain": ["claude-code"] }),
        json!({ "name": "Ada" }),
        json!({ "name": "Ada", "runtime_chain": ["Not A Runtime"] }),
        json!({ "name": "Ada", "runtime_chain": ["claude-code"], "nope": 1 }),
        json!({ "name": "x".repeat(65), "runtime_chain": ["claude-code"] }),
        json!({ "name": "Ada", "runtime_chain": ["claude-code"], "instructions": "x".repeat(20_001) }),
    ] {
        let (status, _) = call(app, "POST", "/api/v1/agents", Some(bad.clone())).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{bad}");
    }
    let body = json!({
        "name": " Ada ",
        "description": "Fixes bugs",
        "instructions": "Write a failing test first.",
        "runtime_chain": ["claude-code", "codex"],
        "skill_ids": ["tdd"],
    });
    let (status, ada) = call(app, "POST", "/api/v1/agents", Some(body)).await;
    assert_eq!(status, StatusCode::CREATED, "{ada}");
    assert_eq!(
        (&ada["name"], &ada["skill_ids"], &ada["task_count"]),
        (&json!("Ada"), &json!(["tdd"]), &json!(0))
    );
    // No host has either runtime: not ready, and why.
    assert_eq!(ada["ready"], false);
    let reason = ada["ready_reason"].as_str().unwrap();
    assert!(
        reason.contains("claude-code") && reason.contains("codex"),
        "{reason}"
    );
    let twin = json!({ "name": "ADA", "runtime_chain": ["codex"] });
    let (status, _) = call(app, "POST", "/api/v1/agents", Some(twin)).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);

    let path = format!("/api/v1/agents/{}", ada["id"].as_str().unwrap());
    let edit = json!({ "description": "Fixes every bug", "runtime_chain": ["codex"] });
    let (status, edited) = call(app, "PATCH", &path, Some(edit)).await;
    assert_eq!(status, StatusCode::OK, "{edited}");
    assert_eq!(
        (&edited["description"], &edited["runtime_chain"]),
        (&json!("Fixes every bug"), &json!(["codex"]))
    );

    // Bob (no manage_runtimes) reads but can't write.
    let bob = member_token(&k.store, "bob", &[GroupId::VIEWER, GroupId::MEMBER]).await;
    let (status, listed) = call_as(app, &bob, "GET", "/api/v1/agents", None).await;
    assert_eq!(
        (status, listed.as_array().unwrap().len()),
        (StatusCode::OK, 1)
    );
    let create = json!({ "name": "Bob", "runtime_chain": ["codex"] });
    let (status, _) = call_as(app, &bob, "POST", "/api/v1/agents", Some(create)).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, _) = call_as(app, &bob, "PATCH", &path, Some(json!({ "name": "B" }))).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, _) = call_as(app, &bob, "DELETE", &path, None).await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    // DELETE archives: off the list; archiving again changes nothing.
    let (status, _) = call(app, "DELETE", &path, None).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (status, _) = call(app, "DELETE", &path, None).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    // Each change is audited as an agent change, once.
    let (_, audit) = call(app, "GET", "/api/v1/audit", None).await;
    let mut changes: Vec<&str> = audit
        .as_array()
        .unwrap()
        .iter()
        .filter(|a| a["action"] == "agent_change" && a["target"] == "Ada")
        .map(|a| a["detail"].as_str().unwrap())
        .collect();
    changes.sort_unstable();
    assert_eq!(changes, ["archived", "created", "updated (was Ada)"]);
    let (_, listed) = call(app, "GET", "/api/v1/agents", None).await;
    assert_eq!(listed, json!([]));
    let (status, _) = call(app, "PATCH", &path, Some(json!({ "name": "Ada" }))).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    let missing = "/api/v1/agents/00000000000000000000000009";
    let (status, _) = call(app, "PATCH", missing, Some(json!({ "name": "Z" }))).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

async fn task(app: &axum::Router, body: Value) -> (StatusCode, Value) {
    call(app, "POST", "/api/v1/tasks", Some(body)).await
}

#[tokio::test]
async fn tasks_take_an_agent_or_workflow_assignee_and_system_workflows_stay_hidden() {
    let k = kernel().await;
    let app = &k.app;
    let body = json!({ "name": "Ada", "runtime_chain": ["claude-code"] });
    let (_, ada) = call(app, "POST", "/api/v1/agents", Some(body)).await;
    let agent = json!({ "kind": "agent", "id": ada["id"] });

    // An agent task in Backlog: assigned, nothing runs yet.
    let (status, view) = task(
        app,
        json!({ "title": "Fix login", "workspace_id": MAIN, "status": "backlog", "assignee": agent }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{view}");
    assert_eq!(view["assignee"], agent);
    assert!(view["run"].is_null());
    assert_eq!(view["workflow"]["name"], "Ada");
    let system = view["workflow"]["id"].as_str().unwrap().to_owned();
    // Its system workflow is nowhere a person picks workflows.
    let (_, workflows) = call(app, "GET", "/api/v1/workflows", None).await;
    assert_eq!(workflows, json!([]));
    let (_, of_main) = call(
        app,
        "GET",
        &format!("/api/v1/workflows?workspace={MAIN}"),
        None,
    )
    .await;
    assert_eq!(of_main, json!([]));
    let (status, _) = call(app, "GET", &format!("/api/v1/workflows/{system}"), None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let set = json!({ "default_workflow_id": system });
    let (status, _) = call(
        app,
        "PATCH",
        &format!("/api/v1/workspaces/{MAIN}"),
        Some(set),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    let (_, listed) = call(app, "GET", "/api/v1/agents", None).await;
    assert_eq!(listed[0]["task_count"], 1);
    let (_, assigned) = call(
        app,
        "GET",
        &format!("/api/v1/tasks?assignee={}", ada["id"].as_str().unwrap()),
        None,
    )
    .await;
    assert_eq!(assigned.as_array().unwrap().len(), 1);

    // Not both, and the shape is checked.
    let (_, workflow) = call(
        app,
        "POST",
        "/api/v1/workflows",
        Some(json!({ "source_yaml": FLOW })),
    )
    .await;
    let both = json!({ "title": "x", "workspace_id": MAIN, "assignee": agent, "workflow_id": workflow["id"] });
    assert_eq!(task(app, both).await.0, StatusCode::UNPROCESSABLE_ENTITY);
    let odd = json!({ "title": "x", "workspace_id": MAIN, "assignee": { "kind": "person", "id": ada["id"] } });
    assert_eq!(task(app, odd).await.0, StatusCode::UNPROCESSABLE_ENTITY);

    // PATCH: a plain task takes a workflow while it hasn't run, and nobody.
    let (_, plain) = task(
        app,
        json!({ "title": "Plain", "workspace_id": MAIN, "status": "backlog" }),
    )
    .await;
    assert!(plain["assignee"].is_null());
    let path = format!("/api/v1/tasks/{}", plain["key"].as_str().unwrap());
    let to_workflow = json!({ "kind": "workflow", "id": workflow["id"] });
    let (status, view) = call(
        app,
        "PATCH",
        &path,
        Some(json!({ "assignee": to_workflow })),
    )
    .await;
    assert_eq!(
        (status, &view["assignee"]),
        (StatusCode::OK, &to_workflow),
        "{view}"
    );
    assert!(view["run"].is_null(), "Backlog: nothing runs");
    let (status, view) = call(app, "PATCH", &path, Some(json!({ "assignee": agent }))).await;
    assert_eq!((status, &view["assignee"]), (StatusCode::OK, &agent));
    let (status, view) = call(app, "PATCH", &path, Some(json!({ "assignee": null }))).await;
    assert_eq!(
        (status, &view["assignee"], &view["workflow"]),
        (StatusCode::OK, &Value::Null, &Value::Null)
    );
    let system_pick = json!({ "kind": "workflow", "id": system });
    let (status, _) = call(
        app,
        "PATCH",
        &path,
        Some(json!({ "assignee": system_pick })),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (_, log) = call(app, "GET", &format!("{path}/activity"), None).await;
    let assignees: Vec<&Value> = log
        .as_array()
        .unwrap()
        .iter()
        .filter(|a| a["kind"] == "assignee")
        .map(|a| &a["body"]["to"])
        .collect();
    assert_eq!(assignees, [&to_workflow, &agent, &Value::Null]);
}

#[tokio::test]
async fn a_hidden_workspaces_workflow_is_no_assignee() {
    let k = kernel().await;
    let app = &k.app;
    let (_, ops) = call(
        app,
        "POST",
        "/api/v1/workspaces",
        Some(json!({ "name": "Ops" })),
    )
    .await;
    let body = json!({ "source_yaml": FLOW, "workspace_id": ops["id"] });
    let (_, on_ops) = call(app, "POST", "/api/v1/workflows", Some(body)).await;
    let bob = member_token(&k.store, "bob", &[GroupId::VIEWER, GroupId::MEMBER]).await;
    let sneak = json!({
        "title": "x",
        "workspace_id": MAIN,
        "assignee": { "kind": "workflow", "id": on_ops["id"] },
    });
    let (status, _) = call_as(app, &bob, "POST", "/api/v1/tasks", Some(sneak)).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}
