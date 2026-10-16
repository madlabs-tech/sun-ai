//! @mentions over HTTP (BLO-128 part 2 test pass): a mention chat takes no
//! key and shows in no task list or count; every refused mention list is
//! 422 and writes nothing; and a member of another workspace reaches
//! neither the task nor its chat, with no secret in any answer.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use axum::Router;
use axum::http::StatusCode;
use serde_json::{Value, json};
use sun_core::model::Role;
use sun_core::{AgentId, TaskId, WorkspaceId};
use sun_store::TaskRepo;

use crate::common::{self, SECRET, TOKEN, kernel};
use crate::workspaces::{call, call_as, member_token_in};

const MAIN: &str = "00000000000000000000000001";

async fn agent(app: &Router, token: &str, workspace: &str, name: &str) -> String {
    let path = format!("/api/v1/agents?workspace={workspace}");
    let body = json!({ "name": name, "runtime_chain": ["claude-code"] });
    let (status, agent) = call_as(app, token, "POST", &path, Some(body)).await;
    assert_eq!(status, StatusCode::CREATED, "{agent}");
    agent["id"].as_str().unwrap().to_owned()
}

/// A Backlog task in `workspace` (`assignee`: an agent id), as `token`.
async fn task(app: &Router, token: &str, workspace: &str, assignee: Option<&str>) -> Value {
    let mut body = json!({ "title": "Fix login", "workspace_id": workspace, "status": "backlog" });
    if let Some(id) = assignee {
        body["assignee"] = json!({ "kind": "agent", "id": id });
    }
    let (status, view) = call_as(app, token, "POST", "/api/v1/tasks", Some(body)).await;
    assert_eq!(status, StatusCode::CREATED, "{view}");
    view
}

async fn comment(app: &Router, token: &str, task: &str, body: Value) -> (StatusCode, Value) {
    let path = format!("/api/v1/tasks/{task}/comments");
    call_as(app, token, "POST", &path, Some(body)).await
}

/// The task's comments, as the token holder (a chat's run moves it on its
/// own, so its status and run entries are not compared).
async fn comments(app: &Router, task: &str) -> Vec<Value> {
    let entries = activity(app, task).await;
    entries
        .into_iter()
        .filter(|e| e["kind"] == "comment")
        .collect()
}

/// The task's activity, as the token holder.
async fn activity(app: &Router, task: &str) -> Vec<Value> {
    let (status, entries) = call(app, "GET", &format!("/api/v1/tasks/{task}/activity"), None).await;
    assert_eq!(status, StatusCode::OK, "{entries}");
    entries.as_array().unwrap().clone()
}

async fn chat_of(k: &common::Kernel, task: &str, agent: &str) -> Option<String> {
    let (task, agent): (TaskId, AgentId) = (task.parse().unwrap(), agent.parse().unwrap());
    let mut tx = k.store.read().await.unwrap();
    let chat = tx.mention_chat(task, agent).await.unwrap();
    chat.map(|c| c.id.to_string())
}

fn ids(list: &Value) -> Vec<String> {
    let list = list.as_array().unwrap();
    list.iter()
        .map(|t| t["id"].as_str().unwrap().to_owned())
        .collect()
}

/// Case 2: mentioning Bob on Ada's task makes his chat, which takes no
/// number: the next board task takes the next key, and neither task list
/// nor Bob's `task_count` sees the chat. The chat itself reads as keyless.
#[tokio::test]
async fn a_mention_chat_takes_no_key_and_shows_in_no_list_or_count() {
    let k = kernel().await;
    let app = &k.app;
    let ada = agent(app, TOKEN, MAIN, "Ada").await;
    let bob = agent(app, TOKEN, MAIN, "Bob").await;
    let first = task(app, TOKEN, MAIN, Some(&ada)).await;
    let first_id = first["id"].as_str().unwrap();
    let n = first["number"].as_u64().unwrap();
    let ask = json!({ "body": "@Bob thoughts?", "mentions": [bob] });
    let (status, _) = comment(app, TOKEN, first_id, ask).await;
    assert_eq!(status, StatusCode::CREATED);
    let chat = chat_of(&k, first_id, &bob).await.expect("Bob's chat");

    let second = task(app, TOKEN, MAIN, None).await;
    assert_eq!(second["number"], json!(n + 1), "{second}");
    let key = first["key"]
        .as_str()
        .unwrap()
        .replace(&n.to_string(), &(n + 1).to_string());
    assert_eq!(second["key"], json!(key));
    let want = vec![
        first_id.to_owned(),
        second["id"].as_str().unwrap().to_owned(),
    ];
    for path in [
        "/api/v1/tasks".to_owned(),
        format!("/api/v1/tasks?workspace={MAIN}"),
    ] {
        let (status, listed) = call(app, "GET", &path, None).await;
        assert_eq!(status, StatusCode::OK);
        let mut got = ids(&listed);
        got.sort();
        let mut want = want.clone();
        want.sort();
        assert_eq!(got, want, "{path}: {listed}");
    }
    let (_, agents) = call(app, "GET", "/api/v1/agents", None).await;
    let count = |id: &str| {
        let agents = agents.as_array().unwrap();
        agents.iter().find(|a| a["id"] == id).unwrap()["task_count"].clone()
    };
    assert_eq!((count(&ada), count(&bob)), (json!(1), json!(0)));
    // Read by its id, the chat has no key and no number.
    let (status, view) = call(app, "GET", &format!("/api/v1/tasks/{chat}"), None).await;
    assert_eq!(status, StatusCode::OK, "{view}");
    assert_eq!(
        (&view["key"], &view["number"]),
        (&json!(null), &json!(null))
    );
}

/// Case 4: every refused mention list is 422 and leaves no activity entry,
/// no chat and no run behind; the boundary (five, or six that collapse to
/// five) is taken.
#[tokio::test]
async fn refused_mentions_are_422_and_write_nothing() {
    let k = kernel().await;
    let app = &k.app;
    let ada = agent(app, TOKEN, MAIN, "Ada").await;
    let mut helpers = Vec::new();
    for i in 0..6 {
        helpers.push(agent(app, TOKEN, MAIN, &format!("Helper {i}")).await);
    }
    let gone = agent(app, TOKEN, MAIN, "Gone").await;
    let (status, _) = call(app, "DELETE", &format!("/api/v1/agents/{gone}"), None).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let owner = member_token_in(&k.store, "olga", &[]).await;
    let (status, ops) = call_as(
        app,
        &owner,
        "POST",
        "/api/v1/workspaces",
        Some(json!({ "name": "Ops" })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{ops}");
    let ops = ops["id"].as_str().unwrap().to_owned();
    let elsewhere = agent(app, &owner, &ops, "Opsie").await;
    let view = task(app, TOKEN, MAIN, Some(&ada)).await;
    let id = view["id"].as_str().unwrap().to_owned();

    // A chat to mention inside: Helper 0's.
    let (status, _) = comment(
        app,
        TOKEN,
        &id,
        json!({ "body": "@Helper 0", "mentions": [helpers[0]] }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let chat = chat_of(&k, &id, &helpers[0]).await.unwrap();

    let six = json!(helpers);
    let refused: Vec<(&str, &str, Value)> = vec![
        (
            "unknown id",
            &id,
            json!({ "body": "hi", "mentions": ["00000000000000000000000009"] }),
        ),
        (
            "malformed id",
            &id,
            json!({ "body": "hi", "mentions": ["not an id"] }),
        ),
        (
            "not a list",
            &id,
            json!({ "body": "hi", "mentions": helpers[1] }),
        ),
        (
            "six agents",
            &id,
            json!({ "body": "all of you", "mentions": six }),
        ),
        (
            "archived agent",
            &id,
            json!({ "body": "hi", "mentions": [gone] }),
        ),
        (
            "another workspace's agent",
            &id,
            json!({ "body": "hi", "mentions": [elsewhere] }),
        ),
        (
            "one bad among good",
            &id,
            json!({ "body": "hi", "mentions": [helpers[1], gone] }),
        ),
        (
            "empty body",
            &id,
            json!({ "body": "  ", "mentions": [helpers[1]] }),
        ),
        (
            "an agent's fields",
            &id,
            json!({ "body": "hi", "by_agent": helpers[1], "run_id": "x" }),
        ),
        (
            "inside a chat",
            &chat,
            json!({ "body": "@Ada", "mentions": [ada] }),
        ),
    ];
    let before_task = activity(app, &id).await;
    let before_chat = comments(app, &chat).await;
    for (case, on, body) in refused {
        let (status, answer) = comment(app, TOKEN, on, body).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{case}: {answer}");
        assert_eq!(
            activity(app, &id).await,
            before_task,
            "{case}: task activity"
        );
        assert_eq!(
            comments(app, &chat).await,
            before_chat,
            "{case}: chat comments"
        );
        for helper in &helpers[1..] {
            assert!(
                chat_of(&k, &id, helper).await.is_none(),
                "{case}: a chat for {helper}"
            );
        }
        assert!(chat_of(&k, &id, &gone).await.is_none(), "{case}");
        assert!(chat_of(&k, &id, &ada).await.is_none(), "{case}");
    }
    let (_, runs) = call(app, "GET", &format!("/api/v1/tasks/{id}/runs"), None).await;
    assert_eq!(
        runs,
        json!([]),
        "Ada was never woken: her task is in Backlog with no run"
    );

    // Five distinct, and six ids that collapse to five, are taken.
    let five = json!(helpers[1..].to_vec());
    let (status, entry) =
        comment(app, TOKEN, &id, json!({ "body": "five", "mentions": five })).await;
    assert_eq!(status, StatusCode::CREATED, "{entry}");
    assert_eq!(entry["body"]["mentions"], five);
    let twice = [
        &helpers[1],
        &helpers[1],
        &helpers[2],
        &helpers[3],
        &helpers[4],
        &helpers[5],
    ];
    let (status, entry) = comment(
        app,
        TOKEN,
        &id,
        json!({ "body": "again", "mentions": twice }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{entry}");
    assert_eq!(entry["body"]["mentions"], five);
}

/// Case 6: a member of Beta only reaches neither Main's task nor its chat
/// (by id or key: 404), cannot mention into either, and cannot mention
/// Main's agents into Beta; a viewer of Main reads but cannot comment or
/// mention (403). Nothing is written, and no answer holds a secret.
#[tokio::test]
async fn another_workspaces_member_reaches_neither_the_task_nor_its_chat() {
    let k = kernel().await;
    let app = &k.app;
    let ada = agent(app, TOKEN, MAIN, "Ada").await;
    let bob = agent(app, TOKEN, MAIN, "Bob").await;
    let view = task(app, TOKEN, MAIN, Some(&ada)).await;
    let (id, key) = (
        view["id"].as_str().unwrap().to_owned(),
        view["key"].as_str().unwrap().to_owned(),
    );
    let (status, _) = comment(
        app,
        TOKEN,
        &id,
        json!({ "body": "@Bob?", "mentions": [bob] }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let chat = chat_of(&k, &id, &bob).await.unwrap();

    let carol = member_token_in(&k.store, "carol", &[]).await;
    let (status, beta) = call_as(
        app,
        &carol,
        "POST",
        "/api/v1/workspaces",
        Some(json!({ "name": "Beta" })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{beta}");
    let beta = beta["id"].as_str().unwrap().to_owned();
    let betsy = agent(app, &carol, &beta, "Betsy").await;
    let beta_task = task(app, &carol, &beta, Some(&betsy)).await;
    let beta_id = beta_task["id"].as_str().unwrap().to_owned();
    let main: WorkspaceId = MAIN.parse().unwrap();
    let viewer = member_token_in(&k.store, "vic", &[(main, Role::Viewer)]).await;

    let before_task = activity(app, &id).await;
    let before_chat = comments(app, &chat).await;
    let mut answers = Vec::new();
    for target in [&id, &key, &chat] {
        for (method, path, body) in [
            ("GET", format!("/api/v1/tasks/{target}"), None),
            ("GET", format!("/api/v1/tasks/{target}/activity"), None),
            ("GET", format!("/api/v1/tasks/{target}/runs"), None),
            (
                "POST",
                format!("/api/v1/tasks/{target}/comments"),
                Some(json!({ "body": "hi" })),
            ),
            (
                "POST",
                format!("/api/v1/tasks/{target}/comments"),
                Some(json!({ "body": "@Betsy", "mentions": [betsy] })),
            ),
            (
                "POST",
                format!("/api/v1/tasks/{target}/comments"),
                Some(json!({ "body": "@Bob", "mentions": [bob] })),
            ),
            (
                "PATCH",
                format!("/api/v1/tasks/{target}"),
                Some(json!({ "title": "mine" })),
            ),
        ] {
            let (status, answer) = call_as(app, &carol, method, &path, body).await;
            assert_eq!(
                status,
                StatusCode::NOT_FOUND,
                "carol {method} {path}: {answer}"
            );
            answers.push(answer);
        }
    }
    // Main's agents can't be mentioned into Beta.
    let (status, answer) = comment(
        app,
        &carol,
        &beta_id,
        json!({ "body": "@Bob", "mentions": [bob] }),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{answer}");
    answers.push(answer);
    assert!(chat_of(&k, &beta_id, &bob).await.is_none());
    // A viewer of Main reads the task and the chat, writes to neither.
    for target in [&id, &chat] {
        let (status, answer) = call_as(
            app,
            &viewer,
            "GET",
            &format!("/api/v1/tasks/{target}/activity"),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{answer}");
        answers.push(answer);
        for body in [
            json!({ "body": "hi" }),
            json!({ "body": "@Ada", "mentions": [ada] }),
        ] {
            let (status, answer) = comment(app, &viewer, target, body).await;
            assert_eq!(
                status,
                StatusCode::FORBIDDEN,
                "viewer on {target}: {answer}"
            );
            answers.push(answer);
        }
    }
    assert_eq!(activity(app, &id).await, before_task);
    assert_eq!(comments(app, &chat).await, before_chat);
    assert!(chat_of(&k, &id, &ada).await.is_none());
    assert!(chat_of(&k, &id, &betsy).await.is_none());
    let text = serde_json::to_string(&answers).unwrap();
    for secret in [TOKEN, SECRET, carol.as_str(), viewer.as_str()] {
        assert!(
            !text.to_lowercase().contains(&secret.to_lowercase()),
            "{secret} in {text}"
        );
    }
}
