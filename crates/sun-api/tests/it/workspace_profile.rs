//! The workspace profile: slug, description, context, logo, issue prefix and
//! default workflow. Writes need `edit_workspace`; a hidden workspace is 404.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use sun_core::GroupId;

use crate::common::{self, TOKEN, kernel};
use crate::workspaces::{FLOW, call, call_as, member_token};

const PNG: &[u8] = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR";

/// A raw-body request; the status, content type and body.
async fn raw(
    app: &Router,
    token: &str,
    method: &str,
    path: &str,
    content_type: &str,
    body: Vec<u8>,
) -> (StatusCode, String, Vec<u8>) {
    let request = Request::builder()
        .method(method)
        .uri(path)
        .header(header::AUTHORIZATION, format!("Bearer {token}"))
        .header(header::CONTENT_TYPE, content_type)
        .body(Body::from(body))
        .unwrap();
    let response = common::respond(app, request).await;
    let status = response.status();
    let kind = response
        .headers()
        .get(header::CONTENT_TYPE)
        .map(|v| v.to_str().unwrap().to_owned())
        .unwrap_or_default();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (status, kind, bytes.to_vec())
}

async fn create(app: &Router, body: Value) -> (StatusCode, Value) {
    call(app, "POST", "/api/v1/workspaces", Some(body)).await
}

#[tokio::test]
async fn slug_prefix_and_profile_are_validated() {
    let k = kernel().await;
    let app = &k.app;
    let (_, workspaces) = call(app, "GET", "/api/v1/workspaces", None).await;
    assert_eq!(
        (&workspaces[0]["slug"], &workspaces[0]["issue_prefix"]),
        (&json!("main"), &json!("MAIN"))
    );

    // A given slug; the prefix comes from it.
    let (status, ops) = create(app, json!({ "name": "Ops", "slug": "ops-team" })).await;
    assert_eq!(status, StatusCode::CREATED, "{ops}");
    assert_eq!(
        (&ops["slug"], &ops["issue_prefix"], &ops["issue_counter"]),
        (&json!("ops-team"), &json!("OPST"), &json!(0))
    );
    assert_eq!(ops["logo_url"], Value::Null);
    // A default slug from the name, made unique.
    let (_, team) = create(app, json!({ "name": "Ops Team!" })).await;
    assert_eq!(team["slug"], "ops-team-2");
    for slug in ["Ops", "a--b", "-a", "a_b", &"a".repeat(49), "ops-team"] {
        let (status, _) = create(app, json!({ "name": format!("x {slug}"), "slug": slug })).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{slug}");
    }

    let ops_id = ops["id"].as_str().unwrap().to_owned();
    let ops_path = format!("/api/v1/workspaces/{ops_id}");
    // The issue prefix: trimmed and uppercased, 1–10 of A-Z 0-9.
    let prefix = format!("{ops_path}/prefix");
    let (status, saved) = call(
        app,
        "POST",
        &prefix,
        Some(json!({ "issue_prefix": " eng2 " })),
    )
    .await;
    assert_eq!(
        (status, &saved["issue_prefix"]),
        (StatusCode::OK, &json!("ENG2"))
    );
    for bad in ["", "A-B", "ABCDEFGHIJK", "ÉT"] {
        let (status, _) = call(app, "POST", &prefix, Some(json!({ "issue_prefix": bad }))).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{bad}");
    }

    // Profile fields; the slug can't be patched.
    let flow = json!({ "source_yaml": FLOW, "workspace_id": ops_id });
    let (_, on_ops) = call(app, "POST", "/api/v1/workflows", Some(flow)).await;
    let (_, on_main) = call(
        app,
        "POST",
        "/api/v1/workflows",
        Some(json!({ "source_yaml": FLOW })),
    )
    .await;
    let change = json!({
        "description": " Runs the fleet ",
        "context": "Deploy only from main.",
        "default_workflow_id": on_ops["id"],
    });
    let (status, saved) = call(app, "PATCH", &ops_path, Some(change)).await;
    assert_eq!(status, StatusCode::OK, "{saved}");
    assert_eq!(
        (
            &saved["description"],
            &saved["context"],
            &saved["default_workflow_id"]
        ),
        (
            &json!("Runs the fleet"),
            &json!("Deploy only from main."),
            &on_ops["id"]
        )
    );
    for bad in [
        json!({ "context": "x".repeat(8001) }),
        json!({ "description": "x".repeat(1001) }),
        json!({ "default_workflow_id": on_main["id"] }),
        json!({ "default_workflow_id": "01ARZ3NDEKTSV4RRFFQ69G5FAV" }),
        json!({ "slug": "other" }),
        json!({ "name": "Main" }),
    ] {
        let (status, _) = call(app, "PATCH", &ops_path, Some(bad.clone())).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{bad}");
    }
    let clear = Some(json!({ "default_workflow_id": null }));
    let (_, saved) = call(app, "PATCH", &ops_path, clear).await;
    assert_eq!(
        (
            &saved["default_workflow_id"],
            &saved["context"],
            &saved["slug"]
        ),
        (
            &Value::Null,
            &json!("Deploy only from main."),
            &json!("ops-team")
        )
    );
}

#[tokio::test]
async fn profile_writes_need_edit_workspace_and_hidden_workspaces_are_404() {
    let k = kernel().await;
    let app = &k.app;
    let (_, hidden) = create(app, json!({ "name": "Hidden" })).await;
    let hidden = hidden["id"].as_str().unwrap().to_owned();
    let main = sun_core::WorkspaceId::MAIN.to_string();
    let member = member_token(&k.store, "bob", &[GroupId::VIEWER, GroupId::MEMBER]).await;
    for (workspace, want) in [
        (&main, StatusCode::FORBIDDEN),
        (&hidden, StatusCode::NOT_FOUND),
    ] {
        let path = format!("/api/v1/workspaces/{workspace}");
        let writes = [
            ("PATCH", path.clone(), Some(json!({ "description": "x" }))),
            (
                "POST",
                format!("{path}/prefix"),
                Some(json!({ "issue_prefix": "X" })),
            ),
            ("DELETE", format!("{path}/logo"), None),
        ];
        for (method, path, body) in writes {
            let (status, _) = call_as(app, &member, method, &path, body).await;
            assert_eq!(status, want, "{method} {path}");
        }
        let logo = format!("{path}/logo");
        let (status, _, _) = raw(app, &member, "PUT", &logo, "image/png", PNG.to_vec()).await;
        assert_eq!(status, want, "PUT {logo}");
    }
    // Reading a hidden workspace's logo is 404 too, even when it has one.
    let logo = format!("/api/v1/workspaces/{hidden}/logo");
    let (status, _, _) = raw(app, TOKEN, "PUT", &logo, "image/png", PNG.to_vec()).await;
    assert_eq!(status, StatusCode::OK);
    let (status, _, _) = raw(app, &member, "GET", &logo, "", Vec::new()).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn logo_is_checked_stored_and_served() {
    let k = kernel().await;
    let app = &k.app;
    let path = format!("/api/v1/workspaces/{}/logo", sun_core::WorkspaceId::MAIN);
    let put = |kind: &'static str, body: Vec<u8>| raw(app, TOKEN, "PUT", &path, kind, body);

    let (status, _, _) = raw(app, TOKEN, "GET", &path, "", Vec::new()).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    for (kind, body) in [
        ("image/jpeg", PNG.to_vec()),
        (
            "image/svg+xml",
            b"<svg xmlns='http://www.w3.org/2000/svg'/>".to_vec(),
        ),
        ("image/png", b"not a png".to_vec()),
        ("image/png", [PNG, &vec![0; 1024 * 1024]].concat()),
    ] {
        let (status, _, _) = put(kind, body).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{kind}");
    }
    let (status, _, body) = put("image/png", PNG.to_vec()).await;
    assert_eq!(status, StatusCode::OK);
    let view: Value = serde_json::from_slice(&body).unwrap();
    let url = view["logo_url"].as_str().unwrap();
    assert!(url.starts_with(&format!("{path}?v=")), "{url}");
    let (status, kind, bytes) = raw(app, TOKEN, "GET", url, "", Vec::new()).await;
    assert_eq!(
        (status, kind.as_str(), bytes.as_slice()),
        (StatusCode::OK, "image/png", PNG)
    );

    // Another workspace with the same image keeps its own copy.
    assert!(view.get("logo_blob").is_none(), "{view}");
    let (_, ops) = call(
        app,
        "POST",
        "/api/v1/workspaces",
        Some(json!({ "name": "Ops" })),
    )
    .await;
    let ops_id = ops["id"].as_str().unwrap().to_owned();
    let ops_logo = format!("/api/v1/workspaces/{ops_id}/logo");
    let (status, _, _) = raw(app, TOKEN, "PUT", &ops_logo, "image/png", PNG.to_vec()).await;
    assert_eq!(status, StatusCode::OK);

    let (status, view) = call(app, "DELETE", &path, None).await;
    assert_eq!((status, &view["logo_url"]), (StatusCode::OK, &Value::Null));
    let (status, _, _) = raw(app, TOKEN, "GET", &path, "", Vec::new()).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _, bytes) = raw(app, TOKEN, "GET", &ops_logo, "", Vec::new()).await;
    assert_eq!(
        (status, bytes.as_slice()),
        (StatusCode::OK, PNG),
        "Ops keeps its own"
    );
    // Replaced, then removed: nothing of it is left.
    let other = [PNG, b"other"].concat();
    let (status, _, body) = raw(app, TOKEN, "PUT", &ops_logo, "image/png", other.clone()).await;
    assert_eq!(status, StatusCode::OK);
    let url = serde_json::from_slice::<Value>(&body).unwrap()["logo_url"].clone();
    assert_ne!(url.as_str(), view["logo_url"].as_str(), "a new version");
    let (_, _, bytes) = raw(app, TOKEN, "GET", &ops_logo, "", Vec::new()).await;
    assert_eq!(bytes, other);
    let (status, _) = call(app, "DELETE", &ops_logo, None).await;
    assert_eq!(status, StatusCode::OK);
    let (status, _, _) = raw(app, TOKEN, "GET", &ops_logo, "", Vec::new()).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    // Deleting a workspace takes its logo with it.
    let (status, _, _) = raw(app, TOKEN, "PUT", &ops_logo, "image/png", PNG.to_vec()).await;
    assert_eq!(status, StatusCode::OK);
    let (status, _) = call(app, "DELETE", &format!("/api/v1/workspaces/{ops_id}"), None).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let mut tx = k.store.read().await.unwrap();
    let ops_id: sun_core::WorkspaceId = ops_id.parse().unwrap();
    assert_eq!(
        sun_store::WorkspaceRepo::logo(&mut tx, ops_id)
            .await
            .unwrap(),
        None
    );
}

#[tokio::test]
async fn workspace_context_rides_in_the_context_pack() {
    let k = kernel().await;
    let app = &k.app;
    let (_, workflow) = call(
        app,
        "POST",
        "/api/v1/workflows",
        Some(json!({ "source_yaml": FLOW })),
    )
    .await;
    let task = json!({ "title": "t", "workflow_id": workflow["id"] });
    let (_, task) = call(app, "POST", "/api/v1/tasks", Some(task)).await;
    let preview = format!(
        "/api/v1/tasks/{}/context/preview",
        task["id"].as_str().unwrap()
    );
    let labels = |pack: &Value| -> Vec<(String, u64, bool)> {
        pack["items"]
            .as_array()
            .unwrap()
            .iter()
            .map(|i| {
                let label = i["label"].as_str().unwrap().to_owned();
                (
                    label,
                    i["priority"].as_u64().unwrap(),
                    i["included"].as_bool().unwrap(),
                )
            })
            .collect()
    };
    let (_, pack) = call(app, "POST", &preview, Some(json!({}))).await;
    assert!(
        labels(&pack)
            .iter()
            .all(|(l, _, _)| l != "Workspace context"),
        "{pack}"
    );

    let main = format!("/api/v1/workspaces/{}", sun_core::WorkspaceId::MAIN);
    let context = json!({ "context": "Deploy only from main." });
    let (status, _) = call(app, "PATCH", &main, Some(context)).await;
    assert_eq!(status, StatusCode::OK);
    let (_, pack) = call(app, "POST", &preview, Some(json!({}))).await;
    assert!(
        labels(&pack).contains(&("Workspace context".into(), 0, true)),
        "{pack}"
    );
}
