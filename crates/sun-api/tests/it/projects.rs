//! Projects through the real kernel: CRUD per workspace; reads need the
//! workspace, writes `run`; a hidden workspace's project is 404.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use axum::http::StatusCode;
use serde_json::{Value, json};
use sun_core::WorkspaceId;
use sun_core::model::Role;

use crate::common::kernel;
use crate::workspaces::{call, call_as, member_token};

#[tokio::test]
async fn projects_crud_with_names_unique_per_workspace() {
    let k = kernel().await;
    let app = &k.app;
    let main = format!("/api/v1/workspaces/{}/projects", WorkspaceId::MAIN);

    let (status, web) = call(app, "POST", &main, Some(json!({ "name": " Web " }))).await;
    assert_eq!(status, StatusCode::CREATED, "{web}");
    assert_eq!(
        (&web["name"], &web["description"]),
        (&json!("Web"), &json!(""))
    );
    let body = json!({ "name": "API", "description": "The public API" });
    let (_, api) = call(app, "POST", &main, Some(body)).await;
    for bad in [
        json!({ "name": "web" }),
        json!({ "name": " " }),
        json!({ "name": "x".repeat(65) }),
        json!({ "name": "x", "description": "x".repeat(1001) }),
    ] {
        let (status, _) = call(app, "POST", &main, Some(bad.clone())).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{bad}");
    }
    // Another workspace may reuse a name.
    let (_, ops) = call(
        app,
        "POST",
        "/api/v1/workspaces",
        Some(json!({ "name": "Ops" })),
    )
    .await;
    let ops = format!(
        "/api/v1/workspaces/{}/projects",
        ops["id"].as_str().unwrap()
    );
    let (status, _) = call(app, "POST", &ops, Some(json!({ "name": "Web" }))).await;
    assert_eq!(status, StatusCode::CREATED);

    let (_, listed) = call(app, "GET", &main, None).await;
    let names: Vec<&Value> = listed
        .as_array()
        .unwrap()
        .iter()
        .map(|p| &p["name"])
        .collect();
    assert_eq!(names, [&json!("API"), &json!("Web")]);

    let api_path = format!("/api/v1/projects/{}", api["id"].as_str().unwrap());
    let (status, saved) = call(app, "PATCH", &api_path, Some(json!({ "name": "Api" }))).await;
    assert_eq!(
        (status, &saved["name"], &saved["description"]),
        (StatusCode::OK, &json!("Api"), &json!("The public API"))
    );
    let (status, _) = call(app, "PATCH", &api_path, Some(json!({ "name": "WEB" }))).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    let (status, _) = call(
        app,
        "PATCH",
        &api_path,
        Some(json!({ "workspace_id": "x" })),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);

    let web_path = format!("/api/v1/projects/{}", web["id"].as_str().unwrap());
    let (status, _) = call(app, "DELETE", &web_path, None).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (status, _) = call(app, "DELETE", &web_path, None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _) = call(app, "GET", "/api/v1/workspaces/nope/projects", None).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
}

#[tokio::test]
async fn project_writes_need_run_and_hidden_workspaces_are_404() {
    let k = kernel().await;
    let app = &k.app;
    let main = format!("/api/v1/workspaces/{}/projects", WorkspaceId::MAIN);
    let (_, on_main) = call(app, "POST", &main, Some(json!({ "name": "Web" }))).await;
    let on_main = format!("/api/v1/projects/{}", on_main["id"].as_str().unwrap());
    let (_, hidden) = call(
        app,
        "POST",
        "/api/v1/workspaces",
        Some(json!({ "name": "Hidden" })),
    )
    .await;
    let hidden = format!(
        "/api/v1/workspaces/{}/projects",
        hidden["id"].as_str().unwrap()
    );
    let (_, on_hidden) = call(app, "POST", &hidden, Some(json!({ "name": "Secret" }))).await;
    let on_hidden = format!("/api/v1/projects/{}", on_hidden["id"].as_str().unwrap());

    // A viewer reads but can't write.
    let viewer = member_token(&k.store, "vic", Role::Viewer).await;
    let (status, listed) = call_as(app, &viewer, "GET", &main, None).await;
    assert_eq!(
        (status, listed.as_array().unwrap().len()),
        (StatusCode::OK, 1)
    );
    let writes = [
        ("POST", main.clone(), Some(json!({ "name": "New" }))),
        ("PATCH", on_main.clone(), Some(json!({ "name": "New" }))),
        ("DELETE", on_main.clone(), None),
    ];
    for (method, path, body) in writes {
        let (status, _) = call_as(app, &viewer, method, &path, body).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{method} {path}");
    }

    // A member with `run` doesn't even see a hidden workspace's projects.
    let member = member_token(&k.store, "bob", Role::Member).await;
    let calls = [
        ("GET", hidden.clone(), None),
        ("POST", hidden.clone(), Some(json!({ "name": "New" }))),
        ("PATCH", on_hidden.clone(), Some(json!({ "name": "New" }))),
        ("DELETE", on_hidden.clone(), None),
    ];
    for (method, path, body) in calls {
        let (status, _) = call_as(app, &member, method, &path, body).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{method} {path}");
    }
    let (status, _) = call_as(
        app,
        &member,
        "PATCH",
        &on_main,
        Some(json!({ "name": "Site" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
}
