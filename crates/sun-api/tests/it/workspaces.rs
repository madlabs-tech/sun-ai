//! Workspaces through the real kernel: CRUD for admins only, audited; workflows,
//! tasks and template installs scoped to a workspace.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use crate::common;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use common::{TOKEN, kernel};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use sun_core::WorkspaceId;
use sun_core::model::Role;
use sun_store::{LoginRepo, MemberRepo};

pub(crate) const FLOW: &str =
    "name: ship\nsteps:\n  - id: approve\n    type: approval\n    who: [role:admin]\n";

pub(crate) async fn call_as(
    app: &Router,
    token: &str,
    method: &str,
    path: &str,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let request = Request::builder()
        .method(method)
        .uri(path)
        .header(header::AUTHORIZATION, format!("Bearer {token}"))
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

pub(crate) async fn call(
    app: &Router,
    method: &str,
    path: &str,
    body: Option<Value>,
) -> (StatusCode, Value) {
    call_as(app, TOKEN, method, path, body).await
}

/// A session token of a new member, `role` in Main.
pub(crate) async fn member_token(store: &sun_store::Store, name: &str, role: Role) -> String {
    member_token_in(store, name, &[(WorkspaceId::MAIN, role)]).await
}

/// A session token of a new member with these `roles`.
pub(crate) async fn member_token_in(
    store: &sun_store::Store,
    name: &str,
    roles: &[(WorkspaceId, Role)],
) -> String {
    use sun_store::MembershipRepo;
    let token = format!("session-{name}");
    let hash: String = Sha256::digest(token.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    let email = format!("{name}@corp.io");
    let mut tx = store.begin().await.unwrap();
    let member = tx
        .upsert_member("https://idp", name, name, Some(&email), false)
        .await
        .unwrap();
    for (workspace, role) in roles {
        tx.add_member(*workspace, member.id, *role).await.unwrap();
    }
    let expires = chrono::Utc::now() + chrono::Duration::hours(1);
    tx.insert_login_session(&hash, member.id, expires)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    token
}

#[tokio::test]
async fn workspace_edit_is_admin_only_and_anyone_creates_one() {
    let k = kernel().await;
    let app = &k.app;

    let (status, workspaces) = call(app, "GET", "/api/v1/workspaces", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(workspaces[0]["name"], "Main");
    let main = workspaces[0]["id"].as_str().unwrap().to_owned();

    // A member sees their workspaces and can't change Main, but creates
    // their own (and owns it).
    let member = member_token(&k.store, "bob", Role::Member).await;
    let (status, workspaces) = call_as(app, &member, "GET", "/api/v1/workspaces", None).await;
    assert_eq!(
        (status, workspaces.as_array().unwrap().len()),
        (StatusCode::OK, 1)
    );
    let body = Some(json!({ "name": "Ops" }));
    let mine = Some(json!({ "name": "Bob's" }));
    let (status, created) = call_as(app, &member, "POST", "/api/v1/workspaces", mine).await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    assert!(created["created_by"].is_string(), "{created}");
    let (_, workspaces) = call_as(app, &member, "GET", "/api/v1/workspaces", None).await;
    assert_eq!(workspaces.as_array().unwrap().len(), 2);
    let path = format!("/api/v1/workspaces/{main}");
    let (status, _) = call_as(app, &member, "DELETE", &path, None).await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    // An admin creates, renames and deletes.
    let admin = member_token(&k.store, "ann", Role::Admin).await;
    let (status, ops) = call_as(app, &admin, "POST", "/api/v1/workspaces", body.clone()).await;
    assert_eq!(
        (status, ops["name"].as_str()),
        (StatusCode::CREATED, Some("Ops"))
    );
    let ops_path = format!("/api/v1/workspaces/{}", ops["id"].as_str().unwrap());
    // Names are trimmed, 1–64 chars, unique ignoring case.
    for name in [json!("  ops "), json!(" "), json!("x".repeat(65))] {
        let (status, _) = call_as(
            app,
            &admin,
            "POST",
            "/api/v1/workspaces",
            Some(json!({ "name": name })),
        )
        .await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{name}");
    }
    let rename = Some(json!({ "name": " Operations " }));
    let (status, renamed) = call_as(app, &admin, "PATCH", &ops_path, rename).await;
    assert_eq!(
        (status, renamed["name"].as_str()),
        (StatusCode::OK, Some("Operations"))
    );
    // Git settings are a patch: the switches it names change, the others
    // keep their value; an unknown key, a list or a bad value is 422.
    let patch = |switches: Value| {
        let body = Some(json!({ "git_settings": switches }));
        call_as(app, &admin, "PATCH", &ops_path, body)
    };
    let (status, saved) = patch(json!({ "move_on_merge": false })).await;
    assert_eq!(status, StatusCode::OK, "{saved}");
    assert_eq!(
        saved["git_settings"],
        json!({ "github_enabled": true, "pr_sidebar": true, "auto_link": true, "move_on_merge": false })
    );
    let (_, saved) = patch(json!({ "auto_link": false })).await;
    assert_eq!(saved["git_settings"]["move_on_merge"], false, "{saved}");
    assert_eq!(saved["git_settings"]["auto_link"], false);
    for bad in [
        json!({ "bogus": true }),
        json!({ "auto_link": false, "bogus": true }),
        json!([]),
        json!("off"),
        json!({ "auto_link": "no" }),
    ] {
        let (status, _) = patch(bad.clone()).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{bad}");
    }
    let (_, listed) = call_as(app, &admin, "GET", "/api/v1/workspaces", None).await;
    let ops_listed = listed
        .as_array()
        .unwrap()
        .iter()
        .find(|w| w["id"] == ops["id"])
        .unwrap();
    assert_eq!(ops_listed["git_settings"]["auto_link"], false, "kept");
    // Main can't be deleted; an empty workspace can.
    let (status, _) = call_as(app, &admin, "DELETE", &path, None).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    let (status, _) = call_as(app, &admin, "DELETE", &ops_path, None).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (status, _) = call_as(
        app,
        &admin,
        "PATCH",
        &ops_path,
        Some(json!({ "name": "x" })),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    let (_, audit) = call(app, "GET", "/api/v1/audit", None).await;
    let details: Vec<(&str, &str)> = audit
        .as_array()
        .unwrap()
        .iter()
        .filter(|e| e["action"] == "workspace_change")
        .map(|e| (e["target"].as_str().unwrap(), e["detail"].as_str().unwrap()))
        .collect();
    for want in [
        ("Ops", "created"),
        ("Operations", "renamed from Ops"),
        ("Operations", "deleted"),
    ] {
        assert!(details.contains(&want), "{want:?} in {details:?}");
    }
}

#[tokio::test]
async fn workspace_scopes_workflows_tasks_and_templates() {
    let k = kernel().await;
    let app = &k.app;
    let (_, ops) = call(
        app,
        "POST",
        "/api/v1/workspaces",
        Some(json!({ "name": "Ops" })),
    )
    .await;
    let ops = ops["id"].as_str().unwrap().to_owned();
    let (_, workspaces) = call(app, "GET", "/api/v1/workspaces", None).await;
    let main = workspaces[0]["id"].as_str().unwrap().to_owned();

    // A workflow on Ops; without workspace_id it goes on Main.
    let body = json!({ "source_yaml": FLOW, "workspace_id": ops });
    let (status, on_ops) = call(app, "POST", "/api/v1/workflows", Some(body)).await;
    assert_eq!(
        (status, on_ops["workspace_id"].as_str()),
        (StatusCode::CREATED, Some(ops.as_str()))
    );
    let (_, on_main) = call(
        app,
        "POST",
        "/api/v1/workflows",
        Some(json!({ "source_yaml": FLOW })),
    )
    .await;
    assert_eq!(on_main["workspace_id"], main.as_str());
    let missing = json!({ "source_yaml": FLOW, "workspace_id": "01ARZ3NDEKTSV4RRFFQ69G5FAV" });
    let (status, _) = call(app, "POST", "/api/v1/workflows", Some(missing)).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    // A new version stays on the workflow's workspace.
    let versions = format!(
        "/api/v1/workflows/{}/versions",
        on_ops["id"].as_str().unwrap()
    );
    let moved = json!({ "source_yaml": FLOW, "workspace_id": main });
    let (status, _) = call(app, "POST", &versions, Some(moved)).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    let (status, saved) = call(app, "POST", &versions, Some(json!({ "source_yaml": FLOW }))).await;
    assert_eq!(
        (status, saved["workspace_id"].as_str()),
        (StatusCode::CREATED, Some(ops.as_str()))
    );

    let (_, listed) = call(
        app,
        "GET",
        &format!("/api/v1/workflows?workspace={ops}"),
        None,
    )
    .await;
    let ids: Vec<&Value> = listed
        .as_array()
        .unwrap()
        .iter()
        .map(|w| &w["id"])
        .collect();
    assert_eq!(ids, vec![&on_ops["id"]]);
    let (_, all) = call(app, "GET", "/api/v1/workflows", None).await;
    assert_eq!(all.as_array().unwrap().len(), 2);

    // A task's workspace is its workflow's workspace.
    for workflow in [&on_ops, &on_main] {
        let task = json!({ "title": "t", "workflow_id": workflow["id"] });
        let (status, _) = call(app, "POST", "/api/v1/tasks", Some(task)).await;
        assert_eq!(status, StatusCode::CREATED);
    }
    let (_, tasks) = call(app, "GET", &format!("/api/v1/tasks?workspace={ops}"), None).await;
    let tasks = tasks.as_array().unwrap();
    assert_eq!(tasks.len(), 1);
    assert_eq!(tasks[0]["workflow"]["workspace_id"], ops.as_str());
    let (_, all) = call(app, "GET", "/api/v1/tasks", None).await;
    assert_eq!(all.as_array().unwrap().len(), 2);
    let (status, _) = call(app, "GET", "/api/v1/tasks?workspace=nope", None).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);

    // A workspace with workflows can't be deleted.
    let (status, _) = call(app, "DELETE", &format!("/api/v1/workspaces/{ops}"), None).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);

    // A template installs once per workspace.
    let (_, templates) = call(app, "GET", "/api/v1/templates", None).await;
    let template = templates[0]["id"].as_str().unwrap();
    let install =
        |workspace: &str| format!("/api/v1/templates/{template}/install?workspace={workspace}");
    let (status, installed) = call(app, "POST", &install(&ops), None).await;
    assert_eq!(
        (status, installed["workspace_id"].as_str()),
        (StatusCode::CREATED, Some(ops.as_str()))
    );
    let (status, _) = call(app, "POST", &install(&ops), None).await;
    assert_eq!(status, StatusCode::CONFLICT);
    let (status, _) = call(app, "POST", &install(&main), None).await;
    assert_eq!(status, StatusCode::CREATED);
}

/// A knowledge source on `workspace` holding one doc with `text`; returns its id.
async fn source_with(app: &Router, workspace: &str, name: &str, text: &str) -> String {
    let body = json!({ "kind": "upload", "name": name, "workspace_id": workspace });
    let (status, source) = call(app, "POST", "/api/v1/knowledge/sources", Some(body)).await;
    assert_eq!(status, StatusCode::CREATED, "{source}");
    let id = source["id"].as_str().unwrap().to_owned();
    let doc = json!({
        "external_id": format!("{name}.md"), "title": name, "version": "1",
        "content": { "markdown": format!("# {name}\n\n{text}") },
    });
    let path = format!("/api/v1/knowledge/sources/{id}/docs");
    let (status, _) = call(app, "POST", &path, Some(json!({ "docs": [doc] }))).await;
    assert_eq!(status, StatusCode::OK);
    id
}

#[tokio::test]
async fn workspace_scopes_repos_and_knowledge_and_import_is_admin_only() {
    let k = kernel().await;
    let app = &k.app;
    let (_, workspaces) = call(app, "GET", "/api/v1/workspaces", None).await;
    let main = workspaces[0]["id"].as_str().unwrap().to_owned();
    let (_, x) = call(
        app,
        "POST",
        "/api/v1/workspaces",
        Some(json!({ "name": "X" })),
    )
    .await;
    let x = x["id"].as_str().unwrap().to_owned();

    // A repo added on X is listed on X only; the same URL twice is refused.
    let body = json!({ "url": "https://example.com/x", "description": "The x site" });
    let x_repos = format!("/api/v1/workspaces/{x}/repos");
    let (status, repo) = call(app, "POST", &x_repos, Some(body.clone())).await;
    assert_eq!(status, StatusCode::CREATED, "{repo}");
    assert_eq!(repo["repo"]["description"], "The x site");
    let repo = repo["repo"]["id"].as_str().unwrap().to_owned();
    let (status, again) = call(app, "POST", &x_repos, Some(body)).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(
        again["error"]["message"]
            .as_str()
            .unwrap()
            .contains("This repository is already added."),
        "{again}"
    );
    let (_, on_x) = call(app, "GET", &x_repos, None).await;
    assert_eq!(on_x.as_array().unwrap().len(), 1);
    let (_, on_main) = call(
        app,
        "GET",
        &format!("/api/v1/workspaces/{main}/repos"),
        None,
    )
    .await;
    assert!(on_main.as_array().unwrap().is_empty());

    // Workspace X's knowledge search never returns Main's docs, and the reverse.
    let billing = source_with(app, &main, "billing", "Refunds take 5 days.").await;
    let hr = source_with(app, &x, "hr", "Refunds of travel take 30 days.").await;
    let search =
        |workspace: &str| format!("/api/v1/knowledge/search?q=refunds&workspace={workspace}");
    let (_, hits) = call(app, "GET", &search(&x), None).await;
    let text = hits.to_string();
    assert!(text.contains("hr") && !text.contains("billing"), "{text}");
    let asked = format!("{}&sources={billing}", search(&x));
    let (_, hits) = call(app, "GET", &asked, None).await;
    assert_eq!(hits, json!([]));
    let (_, hits) = call(app, "GET", "/api/v1/knowledge/search?q=refunds", None).await;
    let text = hits.to_string();
    assert!(text.contains("billing") && !text.contains("\"hr"), "{text}");
    let (_, listed) = call(
        app,
        "GET",
        &format!("/api/v1/knowledge/sources?workspace={x}"),
        None,
    )
    .await;
    assert_eq!(listed.as_array().unwrap().len(), 1);

    // Main sees X's knowledge only once an admin imports it.
    let member = member_token(&k.store, "bob", Role::Member).await;
    let import_hr = format!("/api/v1/workspaces/{main}/knowledge_sources/{hr}");
    // A member of Main only doesn't even see what they'd import;
    // `acl_workspace_visibility_matrix` has the 403 case.
    let (status, _) = call_as(app, &member, "POST", &import_hr, None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let x_id: WorkspaceId = x.parse().unwrap();
    let admin = member_token_in(
        &k.store,
        "ann",
        &[(WorkspaceId::MAIN, Role::Admin), (x_id, Role::Admin)],
    )
    .await;
    let (status, _) = call_as(app, &admin, "POST", &import_hr, None).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (_, hits) = call(app, "GET", "/api/v1/knowledge/search?q=refunds", None).await;
    assert!(hits.to_string().contains("travel"), "{hits}");

    // Removing a shared source only takes it off one workspace; the last one deletes it.
    let (status, _) = call(
        app,
        "DELETE",
        &format!("/api/v1/knowledge/sources/{hr}?workspace={x}"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (_, all) = call(app, "GET", "/api/v1/knowledge/sources", None).await;
    assert_eq!(all.as_array().unwrap().len(), 2);
    let (status, _) = call_as(app, &admin, "DELETE", &import_hr, None).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (_, all) = call(app, "GET", "/api/v1/knowledge/sources", None).await;
    assert_eq!(all.as_array().unwrap().len(), 1);
    // A repo is deleted from its workspace's list; through another
    // workspace's path it is not found.
    let (status, _) = call_as(
        app,
        &admin,
        "DELETE",
        &format!("/api/v1/workspaces/{main}/repos/{repo}"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "X's repo isn't on Main");
    let remove = format!("/api/v1/workspaces/{x}/repos/{repo}");
    let (status, _) = call_as(app, &admin, "DELETE", &remove, None).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (_, on_x) = call(app, "GET", &x_repos, None).await;
    assert_eq!(on_x, json!([]));
    let (status, _) = call_as(app, &admin, "DELETE", &remove, None).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "gone");
}
