//! Two tenants with disjoint members on one deployment: the owner of Alpha
//! (every permission there) never reaches anything of Beta's, whether Beta's
//! ids ride in the path, in `?workspace=`, or in a request body that is
//! otherwise about Alpha; and the lists that name no workspace show nothing
//! of Beta's.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use axum::Router;
use axum::http::StatusCode;
use serde_json::{Value, json};
use sun_core::model::{MemoryItem, MemoryScope, MemoryStatus};
use sun_store::MemoryRepo;

use crate::common::{self, TOKEN, kernel};
use crate::route_matrix::until;
use crate::workspaces::{FLOW, call_as, member_token_in};

/// `method path body` as `token`; asserts a success, the answer.
async fn ok(app: &Router, token: &str, method: &str, path: &str, body: Option<Value>) -> Value {
    let (status, value) = call_as(app, token, method, path, body).await;
    assert!(status.is_success(), "{method} {path}: {status} {value}");
    value
}

fn id(value: &Value) -> String {
    value["id"].as_str().expect("an id").to_owned()
}

/// Everything Beta holds that has an id.
struct Beta {
    id: String,
    task: String,
    run: String,
    workflow: String,
    project: String,
    repo: String,
    source: String,
    agent: String,
    memory: String,
    invitation: String,
}

/// Beta, made and filled by bob (its owner) and the token holder.
async fn beta(k: &common::Kernel, bob: &str) -> Beta {
    let app = &k.app;
    let w = id(&ok(
        app,
        bob,
        "POST",
        "/api/v1/workspaces",
        Some(json!({ "name": "Beta" })),
    )
    .await);
    let host = format!("/api/v1/workspaces/{w}/hosts/{}", k.host);
    ok(app, TOKEN, "PUT", &host, None).await;
    let workflow = ok(
        app,
        bob,
        "POST",
        "/api/v1/workflows",
        Some(json!({ "source_yaml": FLOW, "workspace_id": w })),
    )
    .await;
    let task = ok(
        app,
        bob,
        "POST",
        "/api/v1/tasks",
        Some(json!({ "title": "beta secret plan", "workflow_id": workflow["id"] })),
    )
    .await;
    let task = id(&task);
    let run = until(app, &task, "needs_you").await["run"]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let project = ok(
        app,
        bob,
        "POST",
        &format!("/api/v1/workspaces/{w}/projects"),
        Some(json!({ "name": "Beta web" })),
    )
    .await;
    let repo = ok(
        app,
        bob,
        "POST",
        &format!("/api/v1/workspaces/{w}/repos"),
        Some(json!({ "url": "https://example.com/beta/app" })),
    )
    .await;
    let source = ok(
        app,
        bob,
        "POST",
        "/api/v1/knowledge/sources",
        Some(json!({ "kind": "upload", "name": "beta-handbook", "workspace_id": w })),
    )
    .await;
    let agent = ok(
        app,
        bob,
        "POST",
        &format!("/api/v1/agents?workspace={w}"),
        Some(json!({ "name": "Bea", "runtime_chain": ["claude-code"] })),
    )
    .await;
    ok(
        app,
        bob,
        "PUT",
        &format!("/api/v1/secrets/workspace/{w}/B_KEY"),
        Some(json!({ "value": "sk-beta-0123456789" })),
    )
    .await;
    let runtime = json!({ "name": "beta-llm", "source": "byo_key", "config":
        { "endpoint": "http://localhost:11434/v1", "model": "qwen", "key_secret": "B_KEY" } });
    ok(
        app,
        bob,
        "POST",
        &format!("/api/v1/runtimes?workspace={w}"),
        Some(runtime),
    )
    .await;
    ok(
        app,
        bob,
        "POST",
        &format!("/api/v1/catalog/tdd/install?workspace={w}"),
        Some(json!({})),
    )
    .await;
    let invitation = ok(
        app,
        bob,
        "POST",
        &format!("/api/v1/workspaces/{w}/invitations"),
        Some(json!({ "email": "cy@corp.io", "role": "viewer" })),
    )
    .await;
    let item = MemoryItem {
        id: sun_core::MemoryItemId::generate(),
        workspace_id: w.parse().unwrap(),
        scope: MemoryScope::Ecosystem,
        scope_id: None,
        body: "Beta deploys on Fridays.".into(),
        target: None,
        reason: "r".into(),
        source_run: None,
        status: MemoryStatus::Proposed,
        author: "sun".into(),
        reviewed_by: None,
        created_at: chrono::Utc::now(),
    };
    let mut tx = k.store.begin().await.unwrap();
    tx.insert_memory(&item, "beta").await.unwrap();
    tx.commit().await.unwrap();
    Beta {
        id: w,
        task,
        run,
        workflow: id(&workflow),
        project: id(&project),
        repo: repo["repo"]["id"].as_str().unwrap().to_owned(),
        source: id(&source),
        agent: id(&agent),
        memory: item.id.to_string(),
        invitation: invitation["invitation"]["id"].as_str().unwrap().to_owned(),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn isolation_beta_is_404_to_alphas_owner_by_path_query_and_body() {
    let k = kernel().await;
    let app = &k.app;
    let bob = member_token_in(&k.store, "bob", &[]).await;
    let ada = member_token_in(&k.store, "ada", &[]).await;
    let b = beta(&k, &bob).await;
    let a = id(&ok(
        app,
        &ada,
        "POST",
        "/api/v1/workspaces",
        Some(json!({ "name": "Alpha" })),
    )
    .await);
    let a_task = id(&ok(
        app,
        &ada,
        "POST",
        "/api/v1/tasks",
        Some(json!({ "title": "alpha", "workspace_id": a })),
    )
    .await);
    let bob_id = {
        use sun_store::MemberRepo;
        let mut read = k.store.read().await.unwrap();
        let bob = read.member_by_email("bob@corp.io").await.unwrap().unwrap();
        bob.id.to_string()
    };

    let (t, r, wf) = (&b.task, &b.run, &b.workflow);
    let any = || Some(json!({}));
    // Beta's ids in the path, Alpha's own routes naming one of Beta's things.
    let by_path: Vec<(&str, String, Option<Value>)> = vec![
        ("GET", format!("/tasks/{t}"), None),
        (
            "PATCH",
            format!("/tasks/{t}"),
            Some(json!({ "title": "x" })),
        ),
        ("GET", format!("/tasks/{t}/runs"), None),
        ("GET", format!("/tasks/{t}/activity"), None),
        (
            "POST",
            format!("/tasks/{t}/comments"),
            Some(json!({ "body": "hi" })),
        ),
        ("POST", format!("/tasks/{t}/interrupt"), None),
        ("POST", format!("/tasks/{t}/cancel"), None),
        ("POST", format!("/tasks/{t}/pause"), None),
        ("POST", format!("/tasks/{t}/resume"), None),
        ("POST", format!("/tasks/{t}/bump"), None),
        ("POST", format!("/tasks/{t}/approve"), any()),
        ("POST", format!("/tasks/{t}/reject"), any()),
        ("GET", format!("/tasks/{t}/on_limit"), None),
        (
            "PUT",
            format!("/tasks/{t}/budget"),
            Some(json!({ "cost_usd": 1.0 })),
        ),
        (
            "PUT",
            format!("/tasks/{t}/pr_auto_complete"),
            Some(json!({ "disabled": true })),
        ),
        ("GET", format!("/tasks/{t}/artifacts"), None),
        ("GET", format!("/tasks/{t}/corrections"), None),
        ("POST", format!("/tasks/{t}/decision"), any()),
        ("GET", format!("/tasks/{t}/knowledge_flags"), None),
        ("GET", format!("/tasks/{t}/quality"), None),
        ("POST", format!("/tasks/{t}/quality/check"), any()),
        ("GET", format!("/tasks/{t}/previews"), None),
        ("GET", format!("/tasks/{t}/device"), None),
        ("GET", format!("/tasks/{t}/pins"), None),
        ("GET", format!("/tasks/{t}/design"), None),
        ("GET", format!("/tasks/{t}/mcp"), None),
        ("PUT", format!("/tasks/{t}/mcp"), Some(json!([]))),
        ("GET", format!("/tasks/{t}/context"), None),
        ("PUT", format!("/tasks/{t}/context"), any()),
        ("POST", format!("/tasks/{t}/context/preview"), any()),
        ("GET", format!("/runs/{r}"), None),
        ("POST", format!("/runs/{r}/retry"), None),
        (
            "POST",
            format!("/runs/{r}/messages"),
            Some(json!({ "text": "hi" })),
        ),
        ("POST", format!("/runs/{r}/steps/approve/switch"), any()),
        ("POST", format!("/runs/{r}/steps/approve/retry"), None),
        ("GET", format!("/runs/{r}/events"), None),
        (
            "POST",
            format!("/runs/{r}/questions/q/answer"),
            Some(json!({ "answer": "y" })),
        ),
        ("GET", format!("/workflows/{wf}"), None),
        (
            "POST",
            format!("/workflows/{wf}/versions"),
            Some(json!({ "source_yaml": FLOW })),
        ),
        ("POST", format!("/workflows/{wf}/trigger"), any()),
        (
            "PATCH",
            format!("/projects/{}", b.project),
            Some(json!({ "name": "x" })),
        ),
        ("DELETE", format!("/projects/{}", b.project), None),
        (
            "PATCH",
            format!("/repos/{}", b.repo),
            Some(json!({ "description": "x" })),
        ),
        ("DELETE", format!("/workspaces/{a}/repos/{}", b.repo), None),
        ("GET", format!("/knowledge/sources/{}/docs", b.source), None),
        (
            "POST",
            format!("/knowledge/sources/{}/reindex", b.source),
            None,
        ),
        (
            "DELETE",
            format!("/knowledge/sources/{}?workspace={a}", b.source),
            None,
        ),
        (
            "POST",
            format!("/workspaces/{a}/knowledge_sources/{}", b.source),
            None,
        ),
        (
            "PATCH",
            format!("/agents/{}", b.agent),
            Some(json!({ "name": "Mine" })),
        ),
        ("DELETE", format!("/agents/{}", b.agent), None),
        (
            "POST",
            format!("/memory/{}/review", b.memory),
            Some(json!({ "status": "active" })),
        ),
        (
            "PUT",
            format!("/workspaces/{a}/members/{bob_id}"),
            Some(json!({ "role": "viewer" })),
        ),
        ("DELETE", format!("/workspaces/{a}/members/{bob_id}"), None),
        (
            "DELETE",
            format!("/workspaces/{a}/invitations/{}", b.invitation),
            None,
        ),
        ("PUT", format!("/workspaces/{a}/hosts/{}", k.host), None),
        ("DELETE", format!("/workspaces/{a}/hosts/{}", k.host), None),
        (
            "GET",
            format!("/runtimes/beta-llm/readiness?workspace={a}"),
            None,
        ),
        (
            "PUT",
            format!("/runtimes/beta-llm?workspace={a}"),
            Some(json!({ "name": "beta-llm",
            "source": "byo_key", "config": { "endpoint": "http://x/v1", "model": "m" } })),
        ),
        ("DELETE", format!("/catalog/tdd?workspace={a}"), None),
        ("DELETE", format!("/secrets/workspace/{a}/B_KEY"), None),
    ];
    for (method, path, body) in &by_path {
        let path = format!("/api/v1{path}");
        let (status, value) = call_as(app, &ada, method, &path, body.clone()).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{method} {path}: {value}");
    }

    // `?workspace=` Beta, on every per-workspace list and write.
    let w = &b.id;
    for (method, path) in [
        ("GET", format!("/tasks?workspace={w}")),
        ("GET", format!("/workflows?workspace={w}")),
        ("GET", format!("/knowledge/sources?workspace={w}")),
        ("GET", format!("/knowledge/search?q=beta&workspace={w}")),
        ("GET", format!("/agents?workspace={w}")),
        ("GET", format!("/runtimes?workspace={w}")),
        ("GET", format!("/secrets?workspace={w}")),
        ("GET", format!("/memory?workspace={w}")),
        ("GET", format!("/memory/metrics?workspace={w}")),
        ("GET", format!("/hosts?workspace={w}")),
        ("GET", format!("/catalog?workspace={w}")),
        ("GET", format!("/mcp_servers?workspace={w}")),
        ("GET", format!("/entitlement?workspace={w}")),
        ("POST", format!("/templates/ship/install?workspace={w}")),
        ("POST", format!("/catalog/tdd/install?workspace={w}")),
    ] {
        let path = format!("/api/v1{path}");
        let body = (method == "POST").then(|| json!({}));
        let (status, value) = call_as(app, &ada, method, &path, body).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{method} {path}: {value}");
    }

    // Beta's ids smuggled in bodies about Alpha.
    let in_alpha = |extra: Value| {
        let mut body = json!({ "title": "smuggled", "workspace_id": a });
        body.as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        Some(body)
    };
    let task_path = format!("/api/v1/tasks/{a_task}");
    let smuggled: Vec<(&str, String, Option<Value>)> = vec![
        (
            "POST",
            "/api/v1/tasks".into(),
            in_alpha(json!({ "workflow_id": wf })),
        ),
        (
            "POST",
            "/api/v1/tasks".into(),
            in_alpha(json!({ "assignee": { "kind": "workflow", "id": wf } })),
        ),
        (
            "POST",
            "/api/v1/tasks".into(),
            in_alpha(json!({ "assignee": { "kind": "agent", "id": b.agent } })),
        ),
        (
            "POST",
            "/api/v1/tasks".into(),
            in_alpha(json!({ "project_id": b.project })),
        ),
        (
            "PATCH",
            task_path.clone(),
            Some(json!({ "assignee": { "kind": "workflow", "id": wf } })),
        ),
        (
            "PATCH",
            task_path.clone(),
            Some(json!({ "assignee": { "kind": "agent", "id": b.agent } })),
        ),
        (
            "PATCH",
            task_path.clone(),
            Some(json!({ "project_id": b.project })),
        ),
        (
            "POST",
            "/api/v1/eval_sets".into(),
            Some(json!({ "name": "s", "task_ids": [a_task, t] })),
        ),
        (
            "POST",
            "/api/v1/workflows".into(),
            Some(json!({ "source_yaml": FLOW, "workspace_id": w })),
        ),
        (
            "POST",
            "/api/v1/workflows/validate".into(),
            Some(json!({ "source_yaml": FLOW, "workspace_id": w })),
        ),
        (
            "POST",
            "/api/v1/knowledge/sources".into(),
            Some(json!({ "kind": "upload", "name": "x", "workspace_id": w })),
        ),
    ];
    for (method, path, body) in &smuggled {
        let (status, value) = call_as(app, &ada, method, path, body.clone()).await;
        assert_eq!(
            status,
            StatusCode::NOT_FOUND,
            "{method} {path} {body:?}: {value}"
        );
    }
    // A task context naming Beta's source: 404 like a missing one, and
    // never its name.
    let context = format!("/api/v1/tasks/{a_task}/context");
    let pick = Some(json!({ "sources": [b.source] }));
    let (status, value) = call_as(app, &ada, "PUT", &context, pick.clone()).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{value}");
    assert!(
        !value.to_string().contains("beta-handbook"),
        "Beta's source name leaks: {status} {value}"
    );
    let preview = format!("/api/v1/tasks/{a_task}/context/preview");
    let (_, value) = call_as(app, &ada, "POST", &preview, pick).await;
    assert!(!value.to_string().contains("beta-handbook"), "{value}");
    // An agent whose chain names Beta's runtime gets Alpha's own row of
    // that name, never Beta's key or endpoint.
    ok(
        app,
        &ada,
        "POST",
        &format!("/api/v1/agents?workspace={a}"),
        Some(json!({ "name": "Ana", "runtime_chain": ["beta-llm"] })),
    )
    .await;
    let runtimes = ok(
        app,
        &ada,
        "GET",
        &format!("/api/v1/runtimes?workspace={a}"),
        None,
    )
    .await;
    assert!(!runtimes.to_string().contains("B_KEY"), "{runtimes}");
    assert!(!runtimes.to_string().contains("11434"), "{runtimes}");

    // The lists that name no workspace show nothing of Beta's.
    let beta_ids = [
        &b.id,
        t,
        r,
        wf,
        &b.project,
        &b.repo,
        &b.source,
        &b.agent,
        &b.memory,
        &b.invitation,
    ];
    for path in [
        "/workspaces",
        "/tasks",
        "/workflows",
        "/knowledge/sources",
        "/evals",
        "/eval_sets",
        "/me",
        "/me/invitations",
        "/templates",
    ] {
        let path = format!("/api/v1{path}");
        let (status, value) = call_as(app, &ada, "GET", &path, None).await;
        assert_eq!(status, StatusCode::OK, "{path}: {value}");
        let text = value.to_string();
        for id in beta_ids {
            assert!(!text.contains(id.as_str()), "{path} shows {id}: {text}");
        }
        assert!(!text.contains("beta secret plan"), "{path}: {text}");
    }
    // Beta is untouched: bob still sees all of it.
    let (status, view) = call_as(app, &bob, "GET", &format!("/api/v1/tasks/{t}"), None).await;
    assert_eq!(
        (status, &view["title"]),
        (StatusCode::OK, &json!("beta secret plan"))
    );
    let (_, members) = call_as(
        app,
        &bob,
        "GET",
        &format!("/api/v1/workspaces/{w}/members"),
        None,
    )
    .await;
    assert_eq!(members.as_array().unwrap().len(), 1, "{members}");
    // The control: Beta's owner gets past the guard on every one of those
    // routes that names only Beta's things (so none of the 404s above is a
    // mistyped route). The question doesn't exist: its own 404.
    for (method, path, body) in &by_path {
        if path.contains(a.as_str()) || path.contains("/questions/") {
            continue;
        }
        let path = format!("/api/v1{path}");
        let (status, value) = call_as(app, &bob, method, &path, body.clone()).await;
        assert_ne!(
            status,
            StatusCode::NOT_FOUND,
            "bob: {method} {path}: {value}"
        );
    }
    let drop = format!("/api/v1/knowledge/sources/{}?workspace={w}", b.source);
    let (status, _) = call_as(app, &bob, "DELETE", &drop, None).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
}
