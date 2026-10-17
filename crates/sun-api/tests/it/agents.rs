//! Agents over HTTP: CRUD (DELETE archives) behind `manage_runtimes` in
//! the agent's workspace, reads for its members,
//! readiness, assigning tasks, the hidden system workflow, and @mentions.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use axum::http::StatusCode;
use serde_json::{Value, json};
use sun_core::model::Role;

use crate::common::kernel;
use crate::workspaces::{FLOW, call, call_as, member_token};

const MAIN: &str = "00000000000000000000000001";

#[tokio::test]
async fn agents_crud_needs_manage_runtimes_and_members_read_their_workspaces() {
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

    // Bob, a member of Main (no deployment owner), reads Main's agents
    // (naming Main) but writes none.
    let bob = member_token(&k.store, "bob", Role::Member).await;
    let (status, _) = call_as(app, &bob, "GET", "/api/v1/agents", None).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    let of_main = format!("/api/v1/agents?workspace={MAIN}");
    let (status, listed) = call_as(app, &bob, "GET", &of_main, None).await;
    assert_eq!(
        (status, listed[0]["id"].clone()),
        (StatusCode::OK, ada["id"].clone())
    );
    let create = json!({ "name": "Bob", "runtime_chain": ["codex"] });
    let (status, _) = call_as(app, &bob, "POST", "/api/v1/agents", Some(create.clone())).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, _) = call_as(app, &bob, "POST", &of_main, Some(create)).await;
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
async fn a_comment_mentions_agents_of_its_workspace_and_their_chats_stay_off_task_lists() {
    let k = kernel().await;
    let app = &k.app;
    let new_agent = |name: &str| json!({ "name": name, "runtime_chain": ["claude-code"] });
    let (_, ada) = call(app, "POST", "/api/v1/agents", Some(new_agent("Ada"))).await;
    let (_, bob) = call(app, "POST", "/api/v1/agents", Some(new_agent("Bob"))).await;
    let agent = json!({ "kind": "agent", "id": ada["id"] });
    let body = json!({ "title": "Fix login", "workspace_id": MAIN, "status": "backlog", "assignee": agent });
    let (_, view) = task(app, body).await;
    let comments = format!("/api/v1/tasks/{}/comments", view["key"].as_str().unwrap());

    let ask = json!({ "body": "@Bob thoughts?", "mentions": [bob["id"], bob["id"]] });
    let (status, comment) = call(app, "POST", &comments, Some(ask)).await;
    assert_eq!(status, StatusCode::CREATED, "{comment}");
    assert_eq!(comment["body"]["mentions"], json!([bob["id"]]));
    for bad in [json!(["00000000000000000000000009"]), json!(["not an id"])] {
        let (status, _) = call(
            app,
            "POST",
            &comments,
            Some(json!({ "body": "hi", "mentions": bad })),
        )
        .await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    }
    // Bob's mention chat is no task: one task listed, Bob has none assigned.
    let (_, tasks) = call(app, "GET", "/api/v1/tasks", None).await;
    assert_eq!(tasks.as_array().unwrap().len(), 1, "{tasks}");
    let (_, listed) = call(app, "GET", "/api/v1/agents", None).await;
    let count = |name: &str| {
        listed
            .as_array()
            .unwrap()
            .iter()
            .find(|a| a["name"] == name)
            .unwrap()["task_count"]
            .clone()
    };
    assert_eq!((count("Ada"), count("Bob")), (json!(1), json!(0)));
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
    let bob = member_token(&k.store, "bob", Role::Member).await;
    let sneak = json!({
        "title": "x",
        "workspace_id": MAIN,
        "assignee": { "kind": "workflow", "id": on_ops["id"] },
    });
    let (status, _) = call_as(app, &bob, "POST", "/api/v1/tasks", Some(sneak)).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

/// TRD BLO-117 item 8 over HTTP: `/runtimes` carries each `(host, tool)`'s
/// effort catalog for the form; `POST /agents` takes structured chain
/// entries, refuses an effort the host doesn't offer naming the field, and
/// stores and returns the chain unchanged; a workflow YAML with such
/// entries comes back from the API as it was sent.
#[tokio::test]
async fn structured_chain_entries_over_http() {
    let k = kernel().await;
    let app = &k.app;
    let mut hello = crate::api::kernel_host_hello(&k);
    hello["protocol"] = json!("2.1");
    hello["capabilities"]["runtimes"] = json!([{
        "adapter": "fake",
        "readiness": {"installed": true, "version": "1", "logged_in": true, "detail": null},
        "effort_options": [
            {"value": "low", "label": "Low"},
            {"value": "xhigh", "label": "Extra high", "description": "Deepest"},
        ],
        "models": [{"id": "small", "label": "Small", "effort_options": [{"value": "low", "label": "Low"}]}],
    }]);
    let _socket = crate::api::connect_host(app, k.host, hello).await;

    let (_, runtimes) = call(
        app,
        "GET",
        &format!("/api/v1/runtimes?workspace={MAIN}"),
        None,
    )
    .await;
    let fake = runtimes
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["tool"] == "fake")
        .unwrap();
    assert_eq!(fake["effort_options"][1]["label"], "Extra high", "{fake}");
    assert_eq!(fake["models"][0]["id"], "small");

    let host = k.host.to_string();
    for (chain, field) in [
        (
            json!([{"tool": "fake", "effort": "max"}]),
            "runtime_chain[0].effort",
        ),
        (
            json!([{"tool": "fake", "model": "small", "effort": "xhigh"}]),
            "runtime_chain[0].effort",
        ),
        (
            json!([{"tool": "fake", "credential": "nope"}]),
            "runtime_chain[0].credential",
        ),
        (
            json!([{"tool": "fake", "host": "01J000000000000000000HOST0"}]),
            "runtime_chain[0].host",
        ),
    ] {
        let body = json!({ "name": "Ada", "runtime_chain": chain });
        let (status, error) = call(app, "POST", "/api/v1/agents", Some(body)).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{chain}");
        let message = error["error"]["message"].as_str().unwrap();
        assert!(
            message.contains(&format!(" {field}: ")),
            "{chain}: {message}"
        );
    }
    let chain = json!([
        "codex",
        {"tool": "fake", "host": host, "model": "big", "effort": "xhigh"},
    ]);
    let body = json!({ "name": "Ada", "runtime_chain": chain });
    let (status, ada) = call(app, "POST", "/api/v1/agents", Some(body)).await;
    assert_eq!(status, StatusCode::CREATED, "{ada}");
    assert_eq!(ada["runtime_chain"], chain);
    let (_, listed) = call(
        app,
        "GET",
        &format!("/api/v1/agents?workspace={MAIN}"),
        None,
    )
    .await;
    assert_eq!(listed[0]["runtime_chain"], chain);
    assert_eq!(listed[0]["ready"], true, "{listed}");

    // YAML → API → stored → YAML unchanged.
    let yaml = format!(
        "name: pinned\nruntime:\n  chain:\n    - claude-code\n    - {{tool: fake, host: {host}, effort: low}}\nsteps:\n  - {{id: a, type: agent, prompt: p}}\n"
    );
    let body = json!({ "source_yaml": yaml });
    let (status, saved) = call(app, "POST", "/api/v1/workflows", Some(body)).await;
    assert_eq!(status, StatusCode::CREATED, "{saved}");
    let path = format!("/api/v1/workflows/{}", saved["id"].as_str().unwrap());
    let (_, read) = call(app, "GET", &path, None).await;
    assert_eq!(read["source_yaml"], yaml);
    let bad = yaml.replace("effort: low", "effort: max");
    let (status, error) = call(
        app,
        "POST",
        "/api/v1/workflows",
        Some(json!({ "source_yaml": bad })),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(
        error["error"]["message"]
            .as_str()
            .unwrap()
            .contains("runtime.chain[1].effort"),
        "{error}"
    );
}
