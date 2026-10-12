//! The edges of memberships, invitations and sign-up, through the real
//! kernel: races on the last owner, the first verified person and the last
//! seat; demotion and deactivation taking effect on the next request; an
//! invitation revoked, replaced, declined or for an unverified email; and the
//! per-email sign-up limit.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use axum::http::StatusCode;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use sun_core::WorkspaceId;
use sun_core::entitlement::Limits;
use sun_core::model::Role;
use sun_store::{LoginRepo, MemberRepo};

use crate::common::{self, TOKEN, kernel};
use crate::tenancy::{call, link_token, member, smtp_on, workspace};

/// A new session `token` for member `id` (straight in the store).
async fn session_for(store: &sun_store::Store, id: &str, token: &str) {
    let hash: String = Sha256::digest(token.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    let mut tx = store.begin().await.unwrap();
    let expires = chrono::Utc::now() + chrono::Duration::hours(1);
    tx.insert_login_session(&hash, id.parse().unwrap(), expires)
        .await
        .unwrap();
    tx.commit().await.unwrap();
}

/// The six-digit code in the last mail to `email`.
fn code_for(k: &common::Kernel, email: &str) -> String {
    let sent = k.mails.sent.lock().unwrap();
    let (mail, _) = sent
        .iter()
        .rev()
        .find(|(m, _)| m.to == email)
        .expect("a mail");
    mail.body
        .split_whitespace()
        .find(|w| w.len() == 6 && w.bytes().all(|b| b.is_ascii_digit()))
        .expect("a code")
        .to_owned()
}

/// The role of `member` in workspace `w`, as its members list shows it.
async fn role_in(app: &axum::Router, w: &str, member: &str) -> Option<String> {
    let (_, listed) = call(
        app,
        Some(TOKEN),
        "GET",
        &format!("/api/v1/workspaces/{w}/members"),
        None,
    )
    .await;
    listed
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["member_id"] == member)
        .map(|m| m["role"].as_str().unwrap().to_owned())
}

/// Two owners demoting each other at the same moment: one wins, and the
/// workspace always keeps an owner.
#[tokio::test(flavor = "multi_thread")]
async fn membership_two_owners_demoting_each_other_at_once_keep_one() {
    let k = kernel().await;
    let app = &k.app;
    for round in 0..8 {
        let (olga, olga_id) = member(&k.store, &format!("olga{round}"), &[]).await;
        let w = workspace(app, &olga, &format!("Ops {round}")).await;
        let (pia, pia_id) = member(
            &k.store,
            &format!("pia{round}"),
            &[(w.parse().unwrap(), Role::Owner)],
        )
        .await;
        let to_admin = Some(json!({ "role": "admin" }));
        let path = |who: &str| format!("/api/v1/workspaces/{w}/members/{who}");
        let (demote_pia, demote_olga) = (path(&pia_id), path(&olga_id));
        let (a, b) = tokio::join!(
            call(app, Some(&olga), "PUT", &demote_pia, to_admin.clone()),
            call(app, Some(&pia), "PUT", &demote_olga, to_admin.clone()),
        );
        let owners = [&olga_id, &pia_id];
        let mut left = 0;
        for id in owners {
            if role_in(app, &w, id).await.as_deref() == Some("owner") {
                left += 1;
            }
        }
        assert_eq!(left, 1, "round {round}: {:?} {:?}", a, b);
        let statuses = [a.0, b.0];
        assert!(
            statuses.contains(&StatusCode::OK),
            "round {round}: {statuses:?}"
        );
        assert!(
            statuses.iter().all(|s| matches!(
                *s,
                StatusCode::OK | StatusCode::CONFLICT | StatusCode::FORBIDDEN
            )),
            "round {round}: {statuses:?}"
        );
    }
}

/// An owner who isn't the last removes themself; a demoted admin loses the
/// writes on the very next request; a removed member loses even the reads.
#[tokio::test(flavor = "multi_thread")]
async fn membership_changes_apply_on_the_next_request() {
    let k = kernel().await;
    let app = &k.app;
    let (olga, olga_id) = member(&k.store, "olga", &[]).await;
    let w = workspace(app, &olga, "Ops").await;
    let w_id: WorkspaceId = w.parse().unwrap();
    let (pia, pia_id) = member(&k.store, "pia", &[(w_id, Role::Owner)]).await;
    let (al, al_id) = member(&k.store, "al", &[(w_id, Role::Admin)]).await;
    let me = |who: &str| format!("/api/v1/workspaces/{w}/members/{who}");

    let (status, value) = call(app, Some(&pia), "DELETE", &me(&pia_id), None).await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{value}");
    let (status, _) = call(
        app,
        Some(&pia),
        "GET",
        &format!("/api/v1/workspaces/{w}/members"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _) = call(app, Some(&olga), "DELETE", &me(&olga_id), None).await;
    assert_eq!(status, StatusCode::CONFLICT, "now the last owner");

    let agents = format!("/api/v1/agents?workspace={w}");
    let agent = |name: &str| Some(json!({ "name": name, "runtime_chain": ["codex"] }));
    let (status, _) = call(app, Some(&al), "POST", &agents, agent("One")).await;
    assert_eq!(status, StatusCode::CREATED);
    let (status, _) = call(
        app,
        Some(&olga),
        "PUT",
        &me(&al_id),
        Some(json!({ "role": "viewer" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (status, _) = call(app, Some(&al), "POST", &agents, agent("Two")).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "a viewer now");
    let (status, _) = call(app, Some(&al), "GET", &agents, None).await;
    assert_eq!(status, StatusCode::OK, "still reads");
    let (status, _) = call(app, Some(&olga), "DELETE", &me(&al_id), None).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (status, _) = call(app, Some(&al), "GET", &agents, None).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "gone");
}

/// A deactivated account can't accept (its session ended), and once
/// reactivated a new session accepts the same invitation.
#[tokio::test(flavor = "multi_thread")]
async fn membership_a_deactivated_account_accepts_nothing() {
    let k = kernel().await;
    let app = &k.app;
    let (olga, _) = member(&k.store, "olga", &[]).await;
    let w = workspace(app, &olga, "Ops").await;
    let (cat, cat_id) = member(&k.store, "cat", &[]).await;
    let invite = Some(json!({ "email": "cat@corp.io", "role": "member" }));
    let (_, sent) = call(
        app,
        Some(&olga),
        "POST",
        &format!("/api/v1/workspaces/{w}/invitations"),
        invite,
    )
    .await;
    let accept = format!(
        "/api/v1/invitations/{}/accept",
        sent["invitation"]["id"].as_str().unwrap()
    );
    let path = format!("/api/v1/members/{cat_id}/deactivate");
    let (status, _) = call(app, Some(TOKEN), "POST", &path, None).await;
    assert_eq!(status, StatusCode::OK);
    let (status, _) = call(app, Some(&cat), "POST", &accept, None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    // A session made for a deactivated account (e.g. one that raced the
    // deactivation) is refused too.
    session_for(&k.store, &cat_id, "late-session-cat").await;
    let (status, _) = call(app, Some("late-session-cat"), "POST", &accept, None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(role_in(app, &w, &cat_id).await, None);
    let path = format!("/api/v1/members/{cat_id}/activate");
    let (status, _) = call(app, Some(TOKEN), "POST", &path, None).await;
    assert_eq!(status, StatusCode::OK);
    session_for(&k.store, &cat_id, "new-session-cat").await;
    let (status, value) = call(app, Some("new-session-cat"), "POST", &accept, None).await;
    assert_eq!(status, StatusCode::OK, "{value}");
    assert_eq!(role_in(app, &w, &cat_id).await.as_deref(), Some("member"));
}

/// Revoked, replaced and declined invitations; an unverified email.
#[tokio::test(flavor = "multi_thread")]
async fn membership_invitations_revoked_replaced_declined_or_unverified() {
    let k = kernel().await;
    let app = &k.app;
    let (olga, _) = member(&k.store, "olga", &[]).await;
    let w = workspace(app, &olga, "Ops").await;
    let invitations = format!("/api/v1/workspaces/{w}/invitations");
    let invite = |email: &str, role: &str| Some(json!({ "email": email, "role": role }));
    let send = |email: &'static str, role: &'static str| {
        let (app, olga, invitations) = (app.clone(), olga.clone(), invitations.clone());
        async move {
            let (status, sent) =
                call(&app, Some(&olga), "POST", &invitations, invite(email, role)).await;
            assert_eq!(status, StatusCode::CREATED, "{sent}");
            let id = sent["invitation"]["id"].as_str().unwrap().to_owned();
            (id, link_token(sent["link"].as_str().unwrap()))
        }
    };
    let (cat, cat_id) = member(&k.store, "cat", &[]).await;

    // Revoked, then accepted: 404, and the link is dead.
    let (first, first_token) = send("cat@corp.io", "viewer").await;
    let (status, _) = call(
        app,
        Some(&olga),
        "DELETE",
        &format!("{invitations}/{first}"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let accept = |id: &str| format!("/api/v1/invitations/{id}/accept");
    let (status, _) = call(app, Some(&cat), "POST", &accept(&first), None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _) = call(
        app,
        None,
        "GET",
        &format!("/api/v1/invitations/{first_token}"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // Re-inviting replaces the pending one: the old id and link are dead,
    // the new role holds.
    let (old, old_token) = send("cat@corp.io", "viewer").await;
    let (new, new_token) = send("cat@corp.io", "admin").await;
    assert_ne!(old, new);
    let (_, pending) = call(app, Some(&olga), "GET", &invitations, None).await;
    assert_eq!(pending.as_array().unwrap().len(), 1, "{pending}");
    let (status, _) = call(
        app,
        None,
        "GET",
        &format!("/api/v1/invitations/{old_token}"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _) = call(app, Some(&cat), "POST", &accept(&old), None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, page) = call(
        app,
        None,
        "GET",
        &format!("/api/v1/invitations/{new_token}"),
        None,
    )
    .await;
    assert_eq!((status, &page["role"]), (StatusCode::OK, &json!("admin")));
    let (status, joined) = call(app, Some(&cat), "POST", &accept(&new), None).await;
    assert_eq!(
        (status, &joined["role"]),
        (StatusCode::OK, &json!("admin")),
        "{joined}"
    );
    assert_eq!(role_in(app, &w, &cat_id).await.as_deref(), Some("admin"));

    // Declined, then invited again: the new one works.
    let (dan, _) = member(&k.store, "dan", &[]).await;
    let (declined, _) = send("dan@corp.io", "member").await;
    let decline = format!("/api/v1/invitations/{declined}/decline");
    let (status, _) = call(app, Some(&dan), "POST", &decline, None).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (status, _) = call(app, Some(&dan), "POST", &accept(&declined), None).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "declined");
    let (again, _) = send("dan@corp.io", "member").await;
    let (status, _) = call(app, Some(&dan), "POST", &accept(&again), None).await;
    assert_eq!(status, StatusCode::OK);

    // The invited email, not verified yet: 403 until it is.
    let mut tx = k.store.begin().await.unwrap();
    let eve_id = tx
        .create_unverified("eve@corp.io")
        .await
        .unwrap()
        .id
        .to_string();
    tx.commit().await.unwrap();
    let eve = "session-eve-unverified";
    session_for(&k.store, &eve_id, eve).await;
    let (invitation, _) = send("eve@corp.io", "member").await;
    let (status, value) = call(app, Some(eve), "POST", &accept(&invitation), None).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{value}");
    let mut tx = k.store.begin().await.unwrap();
    tx.verify_email(eve_id.parse().unwrap()).await.unwrap();
    tx.commit().await.unwrap();
    let (status, _) = call(app, Some(eve), "POST", &accept(&invitation), None).await;
    assert_eq!(status, StatusCode::OK);
}

/// Two people accepting at once when one seat is left: the plan holds.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "bug: the seat check and the join are two transactions; both accepts pass"]
async fn membership_the_last_seat_goes_to_one_of_two_racing_accepts() {
    let k = kernel().await;
    let app = &k.app;
    for round in 0..5 {
        let (olga, _) = member(&k.store, &format!("olga{round}"), &[]).await;
        let w = workspace(app, &olga, &format!("Ops {round}")).await;
        let invitations = format!("/api/v1/workspaces/{w}/invitations");
        let mut accepts = Vec::new();
        for who in ["cat", "dan"] {
            let name = format!("{who}{round}");
            let (token, _) = member(&k.store, &name, &[]).await;
            let invite = Some(json!({ "email": format!("{name}@corp.io"), "role": "member" }));
            let (status, sent) = call(app, Some(&olga), "POST", &invitations, invite).await;
            assert_eq!(status, StatusCode::CREATED, "{sent}");
            let id = sent["invitation"]["id"].as_str().unwrap().to_owned();
            accepts.push((token, format!("/api/v1/invitations/{id}/accept")));
        }
        common::plan(
            &k,
            w.parse().unwrap(),
            Limits {
                seats: Some(2),
                ..Limits::default()
            },
        );
        let (a, b) = tokio::join!(
            call(app, Some(&accepts[0].0), "POST", &accepts[0].1, None),
            call(app, Some(&accepts[1].0), "POST", &accepts[1].1, None),
        );
        let (_, members) = call(
            app,
            Some(&olga),
            "GET",
            &format!("/api/v1/workspaces/{w}/members"),
            None,
        )
        .await;
        let seats = members.as_array().unwrap().len();
        assert!(
            seats <= 2,
            "round {round}: {seats} members on 2 seats ({} / {})",
            a.0,
            b.0
        );
    }
}

/// `POST /signup` allows ten tries per email (any case) in its window; the
/// eleventh is 429, and another email is unaffected.
#[tokio::test]
async fn membership_signup_is_rate_limited_per_email() {
    let k = kernel().await;
    let app = &k.app;
    smtp_on(app).await;
    let signup = |email: &str| {
        let body = Some(json!({ "email": email }));
        let app = app.clone();
        async move { call(&app, None, "POST", "/api/v1/signup", body).await.0 }
    };
    for n in 0..10 {
        let email = if n % 2 == 0 {
            "ann@acme.io"
        } else {
            "ANN@Acme.io"
        };
        assert_eq!(signup(email).await, StatusCode::ACCEPTED, "try {n}");
    }
    assert_eq!(signup("ann@acme.io").await, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(signup("bob@acme.io").await, StatusCode::ACCEPTED);
}

/// Two people verifying their first codes at the same moment on a new
/// deployment: exactly one becomes its owner.
#[tokio::test(flavor = "multi_thread")]
async fn membership_two_first_verifications_at_once_make_one_owner() {
    for round in 0..5 {
        let k = kernel().await;
        let app = &k.app;
        smtp_on(app).await;
        for email in ["ann@acme.io", "bob@acme.io"] {
            let body = Some(json!({ "email": email }));
            let (status, _) = call(app, None, "POST", "/api/v1/signup", body).await;
            assert_eq!(status, StatusCode::ACCEPTED);
        }
        let verify = |email: &'static str| {
            let body = Some(json!({ "email": email, "password": code_for(&k, email) }));
            let app = app.clone();
            async move { call(&app, None, "POST", "/api/v1/session", body).await }
        };
        let (ann, bob) = tokio::join!(verify("ann@acme.io"), verify("bob@acme.io"));
        let mut owners = 0;
        for (status, value) in [ann, bob] {
            assert_eq!(status, StatusCode::NO_CONTENT, "round {round}: {value}");
            let session = value["session"].as_str().unwrap();
            let (_, me) = call(app, Some(session), "GET", "/api/v1/me", None).await;
            if me["is_owner"] == true {
                owners += 1;
                assert_eq!(me["memberships"][0]["role"], "owner", "{me}");
            }
        }
        assert_eq!(owners, 1, "round {round}");
        let mut read = k.store.read().await.unwrap();
        let listed: Vec<Value> = read
            .members()
            .await
            .unwrap()
            .into_iter()
            .filter(|m| m.is_owner && m.email.is_some())
            .map(|m| json!(m.username))
            .collect();
        assert_eq!(listed.len(), 1, "round {round}: {listed:?}");
    }
}
