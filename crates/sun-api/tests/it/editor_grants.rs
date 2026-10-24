//! Editor grants through the real kernel: owners and admins grant and
//! revoke, everyone else gets 403 there; a non-member gets 404; each member
//! reads their own capabilities on a repo, resolved again on every request.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use axum::http::StatusCode;
use serde_json::{Value, json};
use sun_core::WorkspaceId;
use sun_core::model::Role;
use sun_store::MemberRepo;

use crate::common::{kernel, local_repo};
use crate::workspaces::{call, call_as, member_token_in};

pub(crate) async fn member_id(store: &sun_store::Store, name: &str) -> String {
    let mut read = store.read().await.unwrap();
    let email = format!("{name}@corp.io");
    read.member_by_email(&email)
        .await
        .unwrap()
        .unwrap()
        .id
        .to_string()
}

fn held(value: &(StatusCode, Value)) -> Vec<String> {
    assert_eq!(value.0, StatusCode::OK, "{}", value.1);
    serde_json::from_value(value.1["capabilities"].clone()).unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn editor_grants_resolve_per_request_and_only_owners_and_admins_grant() {
    let k = kernel().await;
    let app = &k.app;
    let made = call(
        app,
        "POST",
        "/api/v1/workspaces",
        Some(json!({ "name": "Ops" })),
    )
    .await;
    let w = made.1["id"].as_str().unwrap().to_owned();
    let w_id: WorkspaceId = w.parse().unwrap();
    let repo = local_repo(&k, w_id, "/srv/ops").await;
    let main_repo = local_repo(&k, WorkspaceId::MAIN, "/srv/main").await;
    let owner = member_token_in(&k.store, "olga", &[(w_id, Role::Owner)]).await;
    let admin = member_token_in(&k.store, "adam", &[(w_id, Role::Admin)]).await;
    let member = member_token_in(&k.store, "mia", &[(w_id, Role::Member)]).await;
    let viewer = member_token_in(&k.store, "vic", &[(w_id, Role::Viewer)]).await;
    let outsider = member_token_in(&k.store, "out", &[]).await;
    let (mia, vic) = (
        member_id(&k.store, "mia").await,
        member_id(&k.store, "vic").await,
    );
    let grants = format!("/api/v1/workspaces/{w}/editor/grants");
    let me = format!("/api/v1/workspaces/{w}/repos/{repo}/editor/me");
    let mine = |token: &str| {
        let (app, token, me) = (app.clone(), token.to_owned(), me.clone());
        async move { held(&call_as(&app, &token, "GET", &me, None).await) }
    };
    let all = vec!["editor.read", "editor.write", "terminal"];

    // Owners hold everything without a grant; nobody else holds anything.
    assert_eq!(mine(&owner).await, all);
    assert_eq!(held(&call(app, "GET", &me, None).await), all, "token");
    for token in [&admin, &member, &viewer] {
        assert!(mine(token).await.is_empty());
    }

    // A non-member: 404 everywhere, never 403.
    for (method, path) in [("GET", &me), ("GET", &grants), ("POST", &grants)] {
        let body = Some(json!({ "subject_type": "all", "capability": "editor.read" }));
        let status = call_as(app, &outsider, method, path, body).await.0;
        assert_eq!(status, StatusCode::NOT_FOUND, "{method} {path}");
    }
    // A repo of another workspace under this one: 404.
    let elsewhere = format!("/api/v1/workspaces/{w}/repos/{main_repo}/editor/me");
    assert_eq!(
        call(app, "GET", &elsewhere, None).await.0,
        StatusCode::NOT_FOUND
    );

    // Members and viewers can't grant (403); admins can (201).
    let read_to_members = json!({
        "subject_type": "role", "subject_id": "member", "capability": "editor.read"
    });
    for token in [&member, &viewer] {
        let status = call_as(app, token, "POST", &grants, Some(read_to_members.clone())).await;
        assert_eq!(status.0, StatusCode::FORBIDDEN, "{}", status.1);
        let status = call_as(app, token, "GET", &grants, None).await.0;
        assert_eq!(status, StatusCode::FORBIDDEN);
    }
    let made = call_as(app, &admin, "POST", &grants, Some(read_to_members.clone())).await;
    assert_eq!(made.0, StatusCode::CREATED, "{}", made.1);
    let role_grant = made.1["id"].as_str().unwrap().to_owned();
    let again = call_as(app, &admin, "POST", &grants, Some(read_to_members)).await;
    assert_eq!(again.0, StatusCode::UNPROCESSABLE_ENTITY, "once");

    // A role grant covers that role and those above it.
    assert_eq!(mine(&member).await, ["editor.read"]);
    assert_eq!(mine(&admin).await, ["editor.read"]);
    assert!(mine(&viewer).await.is_empty());

    // Viewers get `editor.read` at most: the role and a viewer member.
    for body in [
        json!({ "subject_type": "role", "subject_id": "viewer", "capability": "editor.write" }),
        json!({ "subject_type": "member", "subject_id": vic, "capability": "terminal" }),
    ] {
        let refused = call(app, "POST", &grants, Some(body)).await;
        assert_eq!(refused.0, StatusCode::UNPROCESSABLE_ENTITY, "{}", refused.1);
    }
    let to_vic =
        json!({ "subject_type": "member", "subject_id": vic, "capability": "editor.read" });
    assert_eq!(
        call(app, "POST", &grants, Some(to_vic)).await.0,
        StatusCode::CREATED
    );
    assert_eq!(mine(&viewer).await, ["editor.read"]);

    // A member grant scoped to one repo; write implies read, never terminal.
    let to_mia = json!({
        "subject_type": "member", "subject_id": mia, "capability": "editor.write",
        "repo_id": repo,
    });
    let made = call(app, "POST", &grants, Some(to_mia)).await;
    assert_eq!(made.0, StatusCode::CREATED, "{}", made.1);
    let mia_grant = made.1["id"].as_str().unwrap().to_owned();
    assert_eq!(mine(&member).await, ["editor.read", "editor.write"]);
    let foreign = json!({
        "subject_type": "all", "capability": "editor.read", "repo_id": main_repo,
    });
    let status = call_as(app, &owner, "POST", &grants, Some(foreign)).await.0;
    assert_eq!(status, StatusCode::NOT_FOUND, "a repo of another workspace");
    let bad = json!({ "subject_type": "group", "subject_id": "x", "capability": "terminal" });
    assert_eq!(
        call(app, "POST", &grants, Some(bad)).await.0,
        StatusCode::UNPROCESSABLE_ENTITY
    );

    let listed = call_as(app, &admin, "GET", &grants, None).await;
    assert_eq!(listed.0, StatusCode::OK);
    assert_eq!(listed.1.as_array().unwrap().len(), 3);

    // A revoke applies to the next request.
    for grant in [&mia_grant, &role_grant] {
        let path = format!("{grants}/{grant}");
        assert_eq!(
            call_as(app, &admin, "DELETE", &path, None).await.0,
            StatusCode::NO_CONTENT
        );
        assert_eq!(
            call(app, "DELETE", &path, None).await.0,
            StatusCode::NOT_FOUND
        );
    }
    assert!(mine(&member).await.is_empty());

    // Grants and revokes are audited.
    let audit = call(app, "GET", "/api/v1/audit", None).await.1;
    let rows: Vec<&Value> = audit
        .as_array()
        .unwrap()
        .iter()
        .filter(|e| e["action"] == "editor_grant")
        .collect();
    assert_eq!(rows.len(), 5, "{audit}");
    assert!(
        rows.iter()
            .any(|e| e["target"] == "mia" && e["detail"] == "granted editor.write on /srv/ops")
    );
    assert!(rows.iter().any(
        |e| e["target"] == "role:member" && e["detail"] == "revoked editor.read on every repo"
    ));
}
