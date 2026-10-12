//! The task lifecycle over HTTP: plain and workflow tasks, manual moves and
//! their 409s, keys, comments, activity, runs, Stop, and other workspaces'
//! tasks answering 404.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::time::Duration;

use axum::Router;
use axum::http::StatusCode;
use serde_json::{Value, json};
use sun_core::model::Role;

use crate::common::kernel;
use crate::workspaces::{FLOW, call, call_as, member_token};

const MAIN: &str = "00000000000000000000000001";

async fn create(app: &Router, body: Value) -> Value {
    let (status, task) = call(app, "POST", "/api/v1/tasks", Some(body)).await;
    assert_eq!(status, StatusCode::CREATED, "{task}");
    task
}

async fn patch(app: &Router, task: &str, body: Value) -> (StatusCode, Value) {
    call(app, "PATCH", &format!("/api/v1/tasks/{task}"), Some(body)).await
}

/// Moves `task` to `status`; the answer's status, or the 409 message.
async fn move_to(app: &Router, task: &str, status: &str) -> Result<String, String> {
    let (code, view) = patch(app, task, json!({ "status": status })).await;
    match code {
        StatusCode::OK => Ok(view["status"].as_str().unwrap().to_owned()),
        StatusCode::CONFLICT => Err(view["error"]["message"].as_str().unwrap().to_owned()),
        other => panic!("{other}: {view}"),
    }
}

async fn until(app: &Router, task: &str, status: &str) -> Value {
    for _ in 0..300 {
        let (_, view) = call(app, "GET", &format!("/api/v1/tasks/{task}"), None).await;
        if view["status"] == status {
            return view;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let (_, view) = call(app, "GET", &format!("/api/v1/tasks/{task}"), None).await;
    panic!("task {task} never reached {status}: {view}");
}

async fn activity(app: &Router, task: &str) -> Vec<Value> {
    let (status, log) = call(app, "GET", &format!("/api/v1/tasks/{task}/activity"), None).await;
    assert_eq!(status, StatusCode::OK, "{log}");
    log.as_array().unwrap().clone()
}

#[tokio::test]
async fn a_plain_task_moves_freely_but_reaches_done_only_from_needs_you() {
    let k = kernel().await;
    let app = &k.app;
    // No workspace and no workflow: nothing to put it on.
    let (status, _) = call(app, "POST", "/api/v1/tasks", Some(json!({ "title": "x" }))).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    let task = create(
        app,
        json!({ "title": "Write the docs", "workspace_id": MAIN }),
    )
    .await;
    assert_eq!(
        (
            &task["key"],
            &task["status"],
            &task["workflow"],
            &task["priority"]
        ),
        (
            &json!("MAIN-1"),
            &json!("todo"),
            &Value::Null,
            &json!("none")
        )
    );
    let id = task["id"].as_str().unwrap();
    // Keys find it, in any case.
    let (status, by_key) = call(app, "GET", "/api/v1/tasks/main-1", None).await;
    assert_eq!((status, &by_key["id"]), (StatusCode::OK, &task["id"]));
    let (status, _) = call(app, "GET", "/api/v1/tasks/MAIN-99", None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    // Every task route takes the key, query and all.
    let limit = json!({ "on_limit": "switch" });
    let (status, _) = call(app, "PUT", "/api/v1/tasks/main-1/on_limit", Some(limit)).await;
    assert_eq!(status, StatusCode::OK);
    for path in [
        "/api/v1/tasks/MAIN-1/artifacts",
        "/api/v1/tasks/main-1/activity?after=1",
    ] {
        let (status, body) = call(app, "GET", path, None).await;
        assert_eq!((status, body), (StatusCode::OK, json!([])), "{path}");
    }
    let (status, _) = call(app, "GET", "/api/v1/tasks/MAIN-99/artifacts", None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // Free moves among the open statuses (Verifying too: nothing runs).
    for to in ["in_progress", "verifying", "backlog", "blocked", "todo"] {
        assert_eq!(move_to(app, id, to).await, Ok(to.to_owned()), "→ {to}");
    }
    assert_eq!(
        move_to(app, id, "done").await,
        Err("A task moves to Done only from Needs you.".to_owned())
    );
    assert_eq!(move_to(app, id, "needs_you").await, Ok("needs_you".into()));
    assert_eq!(move_to(app, "MAIN-1", "done").await, Ok("done".into()));
    assert_eq!(
        move_to(app, id, "todo").await,
        Err("This task is done; a done task doesn't move.".to_owned())
    );
    let (status, _) = patch(app, id, json!({ "status": "sideways" })).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);

    // Every move is in the log, by whoever made it, in order.
    let log = activity(app, id).await;
    let kinds: Vec<&str> = log.iter().map(|a| a["kind"].as_str().unwrap()).collect();
    assert_eq!(kinds[0], "created");
    assert_eq!(kinds.iter().filter(|k| **k == "status").count(), 7);
    let seqs: Vec<i64> = log.iter().map(|a| a["seq"].as_i64().unwrap()).collect();
    assert_eq!(seqs, (1..=8).collect::<Vec<_>>());
    assert_eq!(
        log[1]["body"],
        json!({ "from": "todo", "to": "in_progress", "by": "user:me" })
    );
    let (_, after) = call(
        app,
        "GET",
        &format!("/api/v1/tasks/{id}/activity?after=7"),
        None,
    )
    .await;
    assert_eq!(after.as_array().unwrap().len(), 1);

    // Fields: title, priority, description, position; each change logged.
    let (status, view) = patch(
        app,
        id,
        json!({ "title": " Docs ", "priority": "urgent", "body": "all of it", "position": 2.5 }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{view}");
    assert_eq!(
        (&view["title"], &view["priority"], &view["position"]),
        (&json!("Docs"), &json!("urgent"), &json!(2.5))
    );
    let kinds: Vec<String> = activity(app, id).await[8..]
        .iter()
        .map(|a| a["kind"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(kinds, ["title", "description", "priority"]);
    for bad in [
        json!({ "title": "" }),
        json!({ "nope": 1 }),
        json!({ "priority": 3 }),
    ] {
        let (status, _) = patch(app, id, bad.clone()).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{bad}");
    }

    // Cancel works on a task that never ran.
    let parked = create(
        app,
        json!({ "title": "Later", "workspace_id": MAIN, "status": "backlog" }),
    )
    .await;
    let path = format!("/api/v1/tasks/{}/cancel", parked["id"].as_str().unwrap());
    let (status, cancelled) = call(app, "POST", &path, None).await;
    assert_eq!(
        (status, &cancelled["status"]),
        (StatusCode::OK, &json!("cancelled"))
    );
    // Stop needs something running.
    let stop = format!("/api/v1/tasks/{id}/interrupt");
    let (status, _) = call(app, "POST", &stop, None).await;
    assert_eq!(status, StatusCode::CONFLICT);
}

#[tokio::test]
async fn comments_are_1_to_10000_characters_and_join_the_activity() {
    let k = kernel().await;
    let app = &k.app;
    let task = create(app, json!({ "title": "Talk", "workspace_id": MAIN })).await;
    let path = format!("/api/v1/tasks/{}/comments", task["key"].as_str().unwrap());
    for bad in [
        json!({ "body": "  " }),
        json!({ "body": "x".repeat(10_001) }),
        json!({}),
    ] {
        let (status, _) = call(app, "POST", &path, Some(bad)).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    }
    let (status, comment) = call(app, "POST", &path, Some(json!({ "body": " Ship it " }))).await;
    assert_eq!(status, StatusCode::CREATED, "{comment}");
    assert_eq!(
        (&comment["kind"], &comment["body"]["text"], &comment["seq"]),
        (&json!("comment"), &json!("Ship it"), &json!(2))
    );
    // The token holder has no member row, but is named: never the engine.
    assert!(comment["actor_member_id"].is_null());
    assert_eq!(comment["body"]["by"], "user:me", "{comment}");
    let long = json!({ "body": "é".repeat(10_000) });
    let (status, _) = call(app, "POST", &path, Some(long)).await;
    assert_eq!(status, StatusCode::CREATED);
}

#[tokio::test]
async fn a_workflow_task_moves_its_run() {
    let k = kernel().await;
    let app = &k.app;
    let (_, workflow) = call(
        app,
        "POST",
        "/api/v1/workflows",
        Some(json!({ "source_yaml": FLOW })),
    )
    .await;
    // `default` needs the workspace's default workflow.
    let default = json!({ "title": "d", "workspace_id": MAIN, "workflow_id": "default" });
    let (status, error) = call(app, "POST", "/api/v1/tasks", Some(default.clone())).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{error}");
    let set = json!({ "default_workflow_id": workflow["id"] });
    let (status, _) = call(
        app,
        "PATCH",
        &format!("/api/v1/workspaces/{MAIN}"),
        Some(set),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let task = create(app, default).await;
    assert_eq!(task["workflow"]["id"], workflow["id"]);
    let id = task["id"].as_str().unwrap();
    // The approval step waits for a person.
    until(app, id, "needs_you").await;
    let (_, runs) = call(app, "GET", &format!("/api/v1/tasks/{id}/runs"), None).await;
    let runs = runs.as_array().unwrap();
    assert_eq!(runs.len(), 1);
    assert_eq!(
        (&runs[0]["attempt"], &runs[0]["trigger"], &runs[0]["status"]),
        (&json!(1), &json!("created"), &json!("waiting_approval"))
    );

    // Done never leaves the run going behind it.
    assert_eq!(
        move_to(app, id, "done").await,
        Err("Approve or cancel the run first.".into())
    );
    // Verifying is the engine's for a task that runs something.
    assert_eq!(
        move_to(app, id, "verifying").await,
        Err("Only the engine moves a task that runs a workflow or an agent to Verifying.".into())
    );
    // Blocked pauses the run; Todo resumes it, and the engine moves it on.
    assert_eq!(move_to(app, id, "blocked").await, Ok("blocked".into()));
    let (_, view) = call(app, "GET", &format!("/api/v1/tasks/{id}"), None).await;
    assert_eq!(view["run"]["end_detail"], "paused by a human");
    assert_eq!(
        move_to(app, id, "in_progress").await,
        Err("A task can't move from Blocked to In progress.".into())
    );
    move_to(app, id, "todo").await.unwrap();
    until(app, id, "needs_you").await;
    // Failed cancels the run and stays Failed.
    assert_eq!(move_to(app, id, "failed").await, Ok("failed".into()));
    let (_, view) = call(app, "GET", &format!("/api/v1/tasks/{id}"), None).await;
    assert_eq!(view["run"]["status"], "cancelled");

    // In Backlog it doesn't run; to Todo queues a run.
    let parked = create(
        app,
        json!({ "title": "p", "workflow_id": workflow["id"], "status": "backlog" }),
    )
    .await;
    let parked_id = parked["id"].as_str().unwrap();
    assert!(parked["run"].is_null());
    move_to(app, parked_id, "todo").await.unwrap();
    until(app, parked_id, "needs_you").await;
    let (_, runs) = call(app, "GET", &format!("/api/v1/tasks/{parked_id}/runs"), None).await;
    assert_eq!(runs[0]["trigger"], "moved");
    // Cancelling it cancels the run; Done stays out of reach of a cancelled task.
    let cancel = format!("/api/v1/tasks/{parked_id}/cancel");
    let (status, view) = call(app, "POST", &cancel, None).await;
    assert_eq!(
        (status, &view["status"]),
        (StatusCode::OK, &json!("cancelled"))
    );
    assert_eq!(view["run"]["status"], "cancelled");
    let (status, _) = call(app, "POST", &cancel, None).await;
    assert_eq!(status, StatusCode::OK, "cancelling twice changes nothing");

    // The log has the engine's moves (no actor) between the person's.
    let log = activity(app, id).await;
    assert!(
        log.iter()
            .any(|a| a["kind"] == "status" && a["actor_member_id"].is_null()),
        "{log:?}"
    );
    assert!(log.iter().any(|a| a["kind"] == "run"), "{log:?}");
}

/// `GET /runs/{id}`: an older run keeps its own steps and gates; another
/// workspace's run answers 404.
#[tokio::test]
async fn any_run_of_a_task_shows_its_steps_and_gates() {
    let k = kernel().await;
    let app = &k.app;
    let (_, workflow) = call(
        app,
        "POST",
        "/api/v1/workflows",
        Some(json!({ "source_yaml": FLOW })),
    )
    .await;
    let task = create(
        app,
        json!({ "title": "Twice", "workflow_id": workflow["id"] }),
    )
    .await;
    let id = task["id"].as_str().unwrap();
    let first = until(app, id, "needs_you").await["run"]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    // Cancel it and run it again: the first run is now an older one.
    move_to(app, id, "failed").await.unwrap();
    let (status, _) = call(app, "POST", &format!("/api/v1/runs/{first}/retry"), None).await;
    assert_eq!(status, StatusCode::OK);
    let mut latest = String::new();
    for _ in 0..300 {
        let (_, view) = call(app, "GET", &format!("/api/v1/tasks/{id}"), None).await;
        latest = view["run"]["id"].as_str().unwrap().to_owned();
        if latest != first && view["run"]["status"] == "waiting_approval" {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_ne!(latest, first);

    let (status, older) = call(app, "GET", &format!("/api/v1/runs/{first}"), None).await;
    assert_eq!(status, StatusCode::OK, "{older}");
    assert_eq!(older["run"]["id"], first.as_str());
    assert_eq!(older["run"]["status"], "cancelled");
    assert_eq!(older["run"]["steps"][0]["step_key"], "approve");
    assert_eq!(older["run"]["steps"][0]["type"], "approval");
    assert_eq!(older["gates"], json!([]));
    let (_, newer) = call(app, "GET", &format!("/api/v1/runs/{latest}"), None).await;
    assert_eq!(newer["run"]["steps"][0]["status"], "waiting_approval");
    let (status, _) = call(app, "GET", "/api/v1/runs/not-a-run", None).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    let missing = "01JZZZZZZZZZZZZZZZZZZZZZZZ";
    let (status, _) = call(app, "GET", &format!("/api/v1/runs/{missing}"), None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // A run on Ops is hidden from bob, who sees only Main.
    let (_, ops) = call(
        app,
        "POST",
        "/api/v1/workspaces",
        Some(json!({ "name": "Ops" })),
    )
    .await;
    let host = format!(
        "/api/v1/workspaces/{}/hosts/{}",
        ops["id"].as_str().unwrap(),
        k.host
    );
    assert_eq!(
        call(app, "PUT", &host, None).await.0,
        StatusCode::NO_CONTENT
    );
    let body = json!({ "source_yaml": FLOW, "workspace_id": ops["id"] });
    let (_, on_ops) = call(app, "POST", "/api/v1/workflows", Some(body)).await;
    let hidden = create(
        app,
        json!({ "title": "secret", "workflow_id": on_ops["id"] }),
    )
    .await;
    let hidden_id = hidden["id"].as_str().unwrap();
    let hidden_run = until(app, hidden_id, "needs_you").await["run"]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let bob = member_token(&k.store, "bob", Role::Member).await;
    let (status, _) = call_as(
        app,
        &bob,
        "GET",
        &format!("/api/v1/runs/{hidden_run}"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _) = call_as(app, &bob, "GET", &format!("/api/v1/runs/{first}"), None).await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test]
async fn another_workspaces_task_answers_404() {
    let k = kernel().await;
    let app = &k.app;
    let (_, ops) = call(
        app,
        "POST",
        "/api/v1/workspaces",
        Some(json!({ "name": "Ops" })),
    )
    .await;
    let ops = ops["id"].as_str().unwrap();
    let hidden = create(app, json!({ "title": "secret", "workspace_id": ops })).await;
    assert_eq!(hidden["key"], "OPS-1");
    let seen = create(app, json!({ "title": "open", "workspace_id": MAIN })).await;
    let bob = member_token(&k.store, "bob", Role::Member).await;
    for task in [hidden["id"].as_str().unwrap(), "OPS-1", "ops-1"] {
        let path = format!("/api/v1/tasks/{task}");
        let (status, _) = call_as(app, &bob, "PATCH", &path, Some(json!({ "title": "x" }))).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{task}");
        let comments = format!("{path}/comments");
        let (status, _) =
            call_as(app, &bob, "POST", &comments, Some(json!({ "body": "hi" }))).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{task}");
        for read in ["", "/activity", "/runs"] {
            let (status, _) = call_as(app, &bob, "GET", &format!("{path}{read}"), None).await;
            assert_eq!(status, StatusCode::NOT_FOUND, "{task}{read}");
        }
    }
    // A task can't be put on a hidden workspace either.
    let body = json!({ "title": "sneak", "workspace_id": ops });
    let (status, _) = call_as(app, &bob, "POST", "/api/v1/tasks", Some(body)).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    // Bob sees and comments on Main's task.
    let path = format!("/api/v1/tasks/{}/comments", seen["key"].as_str().unwrap());
    let (status, comment) = call_as(app, &bob, "POST", &path, Some(json!({ "body": "hi" }))).await;
    assert_eq!(status, StatusCode::CREATED, "{comment}");
    assert!(comment["actor_member_id"].is_string());
    let (_, listed) = call_as(app, &bob, "GET", "/api/v1/tasks", None).await;
    assert_eq!(listed.as_array().unwrap().len(), 1);
    // Prefixes are unique in the ecosystem.
    let prefix = format!("/api/v1/workspaces/{ops}/prefix");
    let (status, error) = call(
        app,
        "POST",
        &prefix,
        Some(json!({ "issue_prefix": "main" })),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(
        error["error"]["message"],
        "invalid input: Another workspace already uses the prefix MAIN."
    );
    // A workspace with tasks can't be deleted.
    let (status, _) = call(app, "DELETE", &format!("/api/v1/workspaces/{ops}"), None).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
}
