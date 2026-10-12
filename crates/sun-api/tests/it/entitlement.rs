//! Plans per workspace through the real kernel: `GET /entitlement` for the
//! members of each workspace, and seats counted per workspace when an
//! invitation is created and accepted.
#![allow(clippy::unwrap_used)]

use crate::common;
use crate::workspaces::{call, call_as, member_token_in};

use axum::http::StatusCode;
use serde_json::{Value, json};
use sun_core::WorkspaceId;
use sun_core::entitlement::Limits;
use sun_core::model::Role;
use sun_store::MemberRepo;

const MAIN: WorkspaceId = WorkspaceId::MAIN;

/// A new workspace `name` made by `token`'s holder.
async fn workspace(app: &axum::Router, token: &str, name: &str) -> WorkspaceId {
    let body = Some(json!({ "name": name }));
    let (status, value) = call_as(app, token, "POST", "/api/v1/workspaces", body).await;
    assert_eq!(status, StatusCode::CREATED, "{value}");
    value["id"].as_str().unwrap().parse().unwrap()
}

fn seats(n: u32) -> Limits {
    Limits {
        seats: Some(n),
        ..Limits::default()
    }
}

#[tokio::test]
async fn entitlement_is_per_workspace_for_its_members() {
    let k = common::kernel().await;
    let app = &k.app;
    let ops = workspace(app, common::TOKEN, "Ops").await;
    common::plan(&k, ops, seats(2));
    let at = |workspace: &str| format!("/api/v1/entitlement?workspace={workspace}");

    // A deployment owner: Main by default (Community here), or any workspace.
    let (status, main) = call(app, "GET", "/api/v1/entitlement", None).await;
    assert_eq!(status, StatusCode::OK, "{main}");
    assert_eq!(
        main,
        json!({ "plan": "community", "paid": false, "status": "community", "valid_until": null,
            "limits": { "seats": null, "runtimes": null, "byo_keys": null, "agents": null, "hosts": null } })
    );
    let (status, paid) = call(app, "GET", &at(&ops.to_string()), None).await;
    assert_eq!(status, StatusCode::OK, "{paid}");
    assert_eq!(
        (&paid["plan"], &paid["paid"], &paid["status"]),
        (&json!("business"), &json!(true), &json!("cloud"))
    );
    assert_eq!(paid["limits"]["seats"], 2);
    assert!(paid["valid_until"].is_string(), "{paid}");
    let (status, _) = call(app, "GET", &at("nope"), None).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    let (status, _) = call(app, "GET", &at(&WorkspaceId::generate().to_string()), None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // A member sees their own workspaces' plans only: Main is hidden to an
    // Ops viewer, with or without naming it.
    let viewer = member_token_in(&k.store, "vic", &[(ops, Role::Viewer)]).await;
    let (status, value) = call_as(app, &viewer, "GET", &at(&ops.to_string()), None).await;
    assert_eq!(
        (status, &value["plan"]),
        (StatusCode::OK, &json!("business"))
    );
    for path in [at(&MAIN.to_string()), "/api/v1/entitlement".into()] {
        let (status, _) = call_as(app, &viewer, "GET", &path, None).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{path}");
    }
    let main_viewer = member_token_in(&k.store, "mia", &[(MAIN, Role::Viewer)]).await;
    let (status, value) = call_as(app, &main_viewer, "GET", "/api/v1/entitlement", None).await;
    assert_eq!(
        (status, &value["plan"]),
        (StatusCode::OK, &json!("community"))
    );
}

#[tokio::test]
async fn seats_count_the_workspace_members_at_invite_and_accept() {
    let k = common::kernel().await;
    let app = &k.app;
    let admin = member_token_in(&k.store, "al", &[]).await;
    let ops = workspace(app, &admin, "Ops").await;
    common::plan(&k, ops, seats(2));
    // Main's crowd doesn't count against Ops.
    for name in ["m1", "m2", "m3"] {
        member_token_in(&k.store, name, &[(MAIN, Role::Member)]).await;
    }
    let invite = |email: &str| Some(json!({ "email": email, "role": "member" }));
    let path = format!("/api/v1/workspaces/{ops}/invitations");
    let (status, sent) = call_as(app, &admin, "POST", &path, invite("cat@corp.io")).await;
    assert_eq!(status, StatusCode::CREATED, "one seat of two: {sent}");
    let invitation = sent["invitation"]["id"].as_str().unwrap().to_owned();

    // A second member fills the plan: no new invitation, and cat can't join.
    member_token_in(&k.store, "bo", &[(ops, Role::Member)]).await;
    let (status, value) = call_as(app, &admin, "POST", &path, invite("dan@corp.io")).await;
    assert_eq!(
        (status, &value["error"]["code"]),
        (StatusCode::FORBIDDEN, &json!("plan_limit"))
    );
    let cat = member_token_in(&k.store, "cat", &[]).await;
    let mut tx = k.store.begin().await.unwrap();
    let cat_id = tx.member_by_email("cat@corp.io").await.unwrap().unwrap().id;
    tx.verify_email(cat_id).await.unwrap();
    tx.commit().await.unwrap();
    let accept = format!("/api/v1/invitations/{invitation}/accept");
    let (status, value): (StatusCode, Value) = call_as(app, &cat, "POST", &accept, None).await;
    assert_eq!(
        (status, &value["error"]["code"]),
        (StatusCode::FORBIDDEN, &json!("plan_limit"))
    );
    assert_eq!(
        value["error"]["message"],
        "Plan limit reached (seats: 2 on the business plan). Remove one or upgrade the plan."
    );

    // An upgrade frees a seat: the same invitation still works.
    common::plan(&k, ops, seats(3));
    let (status, value) = call_as(app, &cat, "POST", &accept, None).await;
    assert_eq!(status, StatusCode::OK, "{value}");
}
