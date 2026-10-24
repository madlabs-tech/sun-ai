//! BLO-192: groups and the terminal policy through the real kernel. Only a
//! deployment owner manages groups and the policy; membership (manual ∪
//! verified email domain) and the policy are read on every request; under
//! `groups` a workspace owner outside the groups loses `terminal` while a
//! deployment owner never does.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use axum::http::StatusCode;
use serde_json::{Value, json};
use sun_core::model::Role;
use sun_core::{MemberId, WorkspaceId};
use sun_store::MemberRepo;

use crate::common::{kernel, local_repo};
use crate::editor_grants::member_id;
use crate::workspaces::{call, call_as, member_token_in};

/// Gives member `id` `email`, verified or not.
async fn email(store: &sun_store::Store, id: &str, email: &str, verified: bool) {
    let id: MemberId = id.parse().unwrap();
    let mut tx = store.begin().await.unwrap();
    tx.set_member_email(id, email).await.unwrap();
    if verified {
        tx.verify_email(id).await.unwrap();
    }
    tx.commit().await.unwrap();
}

fn names(value: &(StatusCode, Value)) -> Vec<String> {
    assert_eq!(value.0, StatusCode::OK, "{}", value.1);
    let list = value.1.as_array().unwrap();
    list.iter()
        .map(|g| g["name"].as_str().unwrap().to_owned())
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn groups_are_owner_only_and_the_terminal_policy_follows_them_per_request() {
    let k = kernel().await;
    let app = &k.app;
    let made = call(
        app,
        "POST",
        "/api/v1/workspaces",
        Some(json!({ "name": "Pub" })),
    )
    .await;
    let w = made.1["id"].as_str().unwrap().to_owned();
    let w_id: WorkspaceId = w.parse().unwrap();
    let repo = local_repo(&k, w_id, "/srv/pub").await;
    // A public sign-up owning their own workspace, a team member, a
    // look-alike domain and an unverified team address.
    let public = member_token_in(&k.store, "pub", &[(w_id, Role::Owner)]).await;
    let team = member_token_in(&k.store, "tia", &[(w_id, Role::Member)]).await;
    let evil = member_token_in(&k.store, "eve", &[(w_id, Role::Member)]).await;
    let fake = member_token_in(&k.store, "fay", &[(w_id, Role::Member)]).await;
    let tia = member_id(&k.store, "tia").await;
    email(&k.store, &tia, "tia@MadLabs.tech", true).await;
    email(
        &k.store,
        &member_id(&k.store, "eve").await,
        "eve@evilmadlabs.tech",
        true,
    )
    .await;
    email(
        &k.store,
        &member_id(&k.store, "fay").await,
        "fay@madlabs.tech",
        false,
    )
    .await;

    // Owner-only: 403 for anyone else, a workspace owner included.
    for token in [&public, &team] {
        for (method, path, body) in [
            ("GET", "/api/v1/groups", None),
            ("POST", "/api/v1/groups", Some(json!({ "name": "x" }))),
            ("GET", "/api/v1/settings/terminal", None),
            (
                "PUT",
                "/api/v1/settings/terminal",
                Some(json!({ "mode": "granted" })),
            ),
        ] {
            let status = call_as(app, token, method, path, body).await.0;
            assert_eq!(status, StatusCode::FORBIDDEN, "{method} {path}");
        }
    }
    let (status, policy) = call(app, "GET", "/api/v1/settings/terminal", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(policy, json!({ "mode": "granted", "groups": [] }));

    let body = json!({ "name": "Team", "domains": ["MadLabs.Tech"] });
    let (status, group) = call(app, "POST", "/api/v1/groups", Some(body)).await;
    assert_eq!(status, StatusCode::CREATED, "{group}");
    assert_eq!(group["domains"], json!(["madlabs.tech"]));
    let g = group["id"].as_str().unwrap().to_owned();
    let (status, _) = call(
        app,
        "POST",
        "/api/v1/groups",
        Some(json!({ "name": "team" })),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "name taken");
    let bad = json!({ "name": "Bad", "domains": ["@madlabs.tech"] });
    let (status, _) = call(app, "POST", "/api/v1/groups", Some(bad)).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "bad domain");

    // Membership: verified email on the domain only.
    let mine = |token: &str| {
        let (app, token) = (app.clone(), token.to_owned());
        async move { names(&call_as(&app, &token, "GET", "/api/v1/groups/mine", None).await) }
    };
    assert_eq!(mine(&team).await, ["Team"]);
    for token in [&public, &evil, &fake] {
        assert!(mine(token).await.is_empty());
    }
    let preview = call(app, "GET", &format!("/api/v1/groups/{g}/preview"), None).await;
    assert_eq!(preview.0, StatusCode::OK);
    let previewed: Vec<&str> = preview
        .1
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["username"].as_str().unwrap())
        .collect();
    assert_eq!(previewed, ["tia"]);
    let other = format!("/api/v1/groups/{g}/preview?domains=evilmadlabs.tech");
    let other = call(app, "GET", &other, None).await;
    assert_eq!(other.1.as_array().unwrap()[0]["username"], "eve");

    // Terminal to the group, then the policy.
    let grants = format!("/api/v1/workspaces/{w}/editor/grants");
    let to_group = json!({ "subject_type": "group", "subject_id": g, "capability": "terminal" });
    let (status, body) = call_as(app, &public, "POST", &grants, Some(to_group)).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let me = format!("/api/v1/workspaces/{w}/repos/{repo}/editor/me");
    let caps = |token: Option<String>| {
        let (app, me) = (app.clone(), me.clone());
        async move {
            let (status, body) = match token {
                Some(t) => call_as(&app, &t, "GET", &me, None).await,
                None => call(&app, "GET", &me, None).await,
            };
            assert_eq!(status, StatusCode::OK, "{body}");
            let caps: Vec<String> = serde_json::from_value(body["capabilities"].clone()).unwrap();
            caps
        }
    };
    let can = |token: Option<String>| {
        let app = app.clone();
        async move {
            let (_, me) = match token {
                Some(t) => call_as(&app, &t, "GET", "/api/v1/me", None).await,
                None => call(&app, "GET", "/api/v1/me", None).await,
            };
            me["can_terminal"] == true
        }
    };
    let all = vec!["editor.read", "editor.write", "terminal"];
    assert_eq!(caps(Some(public.clone())).await, all, "granted mode");
    assert!(can(Some(public.clone())).await);
    assert_eq!(caps(Some(team.clone())).await, ["terminal"]);
    assert!(caps(Some(evil.clone())).await.is_empty());

    let policy = json!({ "mode": "groups", "groups": [g, g] });
    let (status, saved) = call(app, "PUT", "/api/v1/settings/terminal", Some(policy)).await;
    assert_eq!(status, StatusCode::OK, "{saved}");
    assert_eq!(saved["groups"], json!([g]));
    let ghost = json!({ "mode": "groups", "groups": [MemberId::generate().to_string()] });
    let (status, _) = call(app, "PUT", "/api/v1/settings/terminal", Some(ghost)).await;
    assert!(status.is_client_error(), "{status}");

    // The public workspace owner is refused; the team member allowed; the
    // deployment owner always.
    assert_eq!(
        caps(Some(public.clone())).await,
        ["editor.read", "editor.write"]
    );
    assert!(!can(Some(public.clone())).await);
    assert_eq!(caps(Some(team.clone())).await, ["terminal"]);
    assert!(can(Some(team.clone())).await);
    assert!(!can(Some(fake.clone())).await, "unverified");
    assert_eq!(caps(None).await, all);
    assert!(can(None).await);

    // Revoked on the next request: an email moved off the domain.
    email(&k.store, &tia, "tia@gmail.com", true).await;
    assert!(caps(Some(team.clone())).await.is_empty());
    assert!(!can(Some(team.clone())).await);
    // A manual member is in whatever their email.
    let member = format!("/api/v1/groups/{g}/members/{tia}");
    assert_eq!(
        call(app, "POST", &member, None).await.0,
        StatusCode::NO_CONTENT
    );
    assert_eq!(caps(Some(team.clone())).await, ["terminal"]);
    assert_eq!(
        call(app, "DELETE", &member, None).await.0,
        StatusCode::NO_CONTENT
    );
    assert!(caps(Some(team.clone())).await.is_empty());
    assert_eq!(
        call(app, "DELETE", &member, None).await.0,
        StatusCode::NOT_FOUND
    );

    let edit = json!({ "description": "the team", "domains": ["gmail.com"] });
    let (status, edited) = call(app, "PATCH", &format!("/api/v1/groups/{g}"), Some(edit)).await;
    assert_eq!(status, StatusCode::OK, "{edited}");
    assert_eq!(edited["name"], "Team");
    assert_eq!(caps(Some(team.clone())).await, ["terminal"]);

    // Deleting the group takes its grants and its place in the policy.
    let status = call(app, "DELETE", &format!("/api/v1/groups/{g}"), None)
        .await
        .0;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (_, listed) = call_as(app, &public, "GET", &grants, None).await;
    assert_eq!(listed, json!([]));
    let (_, policy) = call(app, "GET", "/api/v1/settings/terminal", None).await;
    assert_eq!(policy, json!({ "mode": "groups", "groups": [] }));
    assert!(!can(Some(team.clone())).await);
    assert!(can(None).await);

    let (_, audit) = call(app, "GET", "/api/v1/audit", None).await;
    let audit = audit.to_string();
    for detail in [
        "group created",
        "group edited",
        "group deleted",
        "member tia added",
        "member tia removed",
        "terminal: groups",
    ] {
        assert!(audit.contains(detail), "audited: {detail}");
    }
}
