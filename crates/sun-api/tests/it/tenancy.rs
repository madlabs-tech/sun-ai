//! Many tenants on one deployment, through the real kernel: sign-up and
//! email verification (the first verified person owns the deployment),
//! workspaces anyone creates, invitations, members and their roles, the
//! per-workspace role on every route, and the routes only deployment
//! owners reach.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use crate::common;

use std::future::IntoFuture;
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use common::{TOKEN, kernel};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use sun_core::model::{Role, SignupPolicy, WorkspaceInvitation};
use sun_core::{InvitationId, WorkspaceId};
use sun_store::{InvitationRepo, LoginRepo, MemberRepo, MembershipRepo};

const MAIN: WorkspaceId = WorkspaceId::MAIN;

/// `method path` as the holder of `token` (`None`: nobody signed in).
pub(crate) async fn call(
    app: &Router,
    token: Option<&str>,
    method: &str,
    path: &str,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let mut request = Request::builder()
        .method(method)
        .uri(path)
        .header(header::CONTENT_TYPE, "application/json");
    if let Some(token) = token {
        request = request.header(header::AUTHORIZATION, format!("Bearer {token}"));
    }
    let request = request
        .body(body.map_or(Body::empty(), |b| Body::from(b.to_string())))
        .unwrap();
    let response = common::respond(app, request).await;
    let status = response.status();
    let cookie = response
        .headers()
        .get_all(header::SET_COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .find_map(|v| v.strip_prefix("sun_token="))
        .and_then(|v| v.split(';').next())
        .filter(|v| !v.is_empty())
        .map(str::to_owned);
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    match cookie {
        Some(cookie) if value.is_null() => (status, json!({ "session": cookie })),
        _ => (status, value),
    }
}

pub(crate) async fn as_token(app: &Router, method: &str, path: &str, body: Option<Value>) -> Value {
    let (status, value) = call(app, Some(TOKEN), method, path, body).await;
    assert!(status.is_success(), "{method} {path}: {status} {value}");
    value
}

pub(crate) async fn smtp_on(app: &Router) {
    let smtp = json!({ "host": "smtp.corp.io", "port": 587, "from": "sun@corp.io",
        "tls": "starttls" });
    as_token(app, "PUT", "/api/v1/smtp", Some(smtp)).await;
}

/// The six-digit code in the last mail.
pub(crate) fn code_in(k: &common::Kernel) -> String {
    k.mails
        .last()
        .body
        .split_whitespace()
        .find(|w| w.len() == 6 && w.bytes().all(|b| b.is_ascii_digit()))
        .expect("a code in the mail")
        .to_owned()
}

/// Signs `email` up, verifies it with the mailed code and sets a password:
/// their session token.
pub(crate) async fn sign_up(k: &common::Kernel, app: &Router, email: &str) -> String {
    let body = Some(json!({ "email": email }));
    let (status, value) = call(app, None, "POST", "/api/v1/signup", body).await;
    assert_eq!(status, StatusCode::ACCEPTED, "{value}");
    let body = Some(json!({ "email": email, "password": code_in(k) }));
    let (status, value) = call(app, None, "POST", "/api/v1/session", body).await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{value}");
    let session = value["session"].as_str().unwrap().to_owned();
    let body = Some(json!({ "password": "a long enough password" }));
    let (status, value) = call(app, Some(&session), "PUT", "/api/v1/me/password", body).await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{value}");
    session
}

/// A session of a new verified member `name` with `roles` (in the store).
pub(crate) async fn member(
    store: &sun_store::Store,
    name: &str,
    roles: &[(WorkspaceId, Role)],
) -> (String, String) {
    let token = format!("session-{name}");
    let hash: String = Sha256::digest(token.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    let mut tx = store.begin().await.unwrap();
    let member = tx
        .upsert_member(
            "https://idp",
            name,
            name,
            Some(&format!("{name}@corp.io")),
            false,
        )
        .await
        .unwrap();
    tx.verify_email(member.id).await.unwrap();
    for (workspace, role) in roles {
        tx.add_member(*workspace, member.id, *role).await.unwrap();
    }
    let expires = chrono::Utc::now() + chrono::Duration::hours(1);
    tx.insert_login_session(&hash, member.id, expires)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    (token, member.id.to_string())
}

/// Workspace `name` made by `token`'s holder: its id.
pub(crate) async fn workspace(app: &Router, token: &str, name: &str) -> String {
    let body = Some(json!({ "name": name }));
    let (status, value) = call(app, Some(token), "POST", "/api/v1/workspaces", body).await;
    assert_eq!(status, StatusCode::CREATED, "{value}");
    value["id"].as_str().unwrap().to_owned()
}

fn with(k: &common::Kernel, policy: SignupPolicy) -> Router {
    sun_api::router(k.api.clone().with_signup_policy(policy))
}

#[tokio::test]
async fn tenancy_signup_follows_the_policy_and_answers_an_existing_account() {
    let k = kernel().await;
    let app = &k.app;
    let signup = |app: Router, email: &str| {
        let body = Some(json!({ "email": email }));
        async move { call(&app, None, "POST", "/api/v1/signup", body).await }
    };
    // No mail server: nobody can sign up yet.
    let (status, _) = signup(app.clone(), "ann@acme.io").await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    smtp_on(app).await;
    let (_, auth) = call(app, None, "GET", "/api/v1/auth", None).await;
    assert_eq!(auth["allow_signup"], true, "{auth}");
    assert_eq!(auth["workspace_creation_disabled"], false, "{auth}");
    let (status, value) = signup(app.clone(), "not an email").await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{value}");
    // Asking twice before verifying mails a new code; once verified, 409.
    let (status, _) = signup(app.clone(), "ann@acme.io").await;
    assert_eq!(status, StatusCode::ACCEPTED);
    sign_up(&k, app, "Ann@Acme.io").await;
    let (status, value) = signup(app.clone(), "ann@acme.io").await;
    assert_eq!(status, StatusCode::CONFLICT, "{value}");
    assert_eq!(value["error"]["code"], "account_exists");
    assert!(
        value["error"]["message"]
            .as_str()
            .unwrap()
            .contains("Sign in instead")
    );

    // Closed: 403 with the copy, unless a pending invitation names the email.
    let closed = with(
        &k,
        SignupPolicy {
            allow_signup: false,
            ..SignupPolicy::default()
        },
    );
    let (_, auth) = call(&closed, None, "GET", "/api/v1/auth", None).await;
    assert_eq!(auth["allow_signup"], false);
    let (status, value) = signup(closed.clone(), "bob@acme.io").await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{value}");
    let message = value["error"]["message"].as_str().unwrap();
    assert!(
        message.contains("closed") && message.contains("invitation"),
        "{message}"
    );
    let invite = Some(json!({ "email": "bob@acme.io", "role": "member" }));
    as_token(
        app,
        "POST",
        &format!("/api/v1/workspaces/{MAIN}/invitations"),
        invite,
    )
    .await;
    let (status, value) = signup(closed.clone(), "bob@acme.io").await;
    assert_eq!(status, StatusCode::ACCEPTED, "invited: {value}");

    // Some domains only.
    let acme = with(
        &k,
        SignupPolicy {
            allowed_email_domains: vec!["acme.io".into()],
            ..SignupPolicy::default()
        },
    );
    let (status, _) = signup(acme.clone(), "eve@else.io").await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, _) = signup(acme.clone(), "cy@acme.io").await;
    assert_eq!(status, StatusCode::ACCEPTED);
}

#[tokio::test]
async fn tenancy_the_first_verified_person_owns_the_deployment_and_main() {
    let k = kernel().await;
    let app = &k.app;
    smtp_on(app).await;
    let ann = sign_up(&k, app, "ann@acme.io").await;
    let (_, me) = call(app, Some(&ann), "GET", "/api/v1/me", None).await;
    assert_eq!(me["is_owner"], true, "{me}");
    assert_eq!(me["email_verified"], true, "{me}");
    assert_eq!(
        me["memberships"][0]["workspace_id"],
        MAIN.to_string(),
        "{me}"
    );
    assert_eq!(me["memberships"][0]["role"], "owner", "{me}");
    assert!(
        me.get("permissions").is_none(),
        "no Main-wide permissions: {me}"
    );
    let bob = sign_up(&k, app, "bob@acme.io").await;
    let (_, me) = call(app, Some(&bob), "GET", "/api/v1/me", None).await;
    assert_eq!(me["is_owner"], false, "{me}");
    assert_eq!(me["memberships"], json!([]), "{me}");
    let (_, listed) = call(app, Some(&bob), "GET", "/api/v1/workspaces", None).await;
    assert_eq!(listed, json!([]), "bob sees no workspace yet");
    // The owner reaches the deployment's own routes; bob doesn't.
    let (status, _) = call(app, Some(&ann), "GET", "/api/v1/members", None).await;
    assert_eq!(status, StatusCode::OK);
    let (status, _) = call(app, Some(&bob), "GET", "/api/v1/members", None).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn tenancy_anyone_creates_a_workspace_unless_the_deployment_says_no() {
    let k = kernel().await;
    let app = &k.app;
    let (bob, bob_id) = member(&k.store, "bob", &[]).await;
    let id = workspace(app, &bob, "Bob's").await;
    let (_, me) = call(app, Some(&bob), "GET", "/api/v1/me", None).await;
    assert_eq!(me["memberships"][0]["workspace_id"], id, "{me}");
    assert_eq!(me["memberships"][0]["role"], "owner", "{me}");
    let (_, members) = call(
        app,
        Some(&bob),
        "GET",
        &format!("/api/v1/workspaces/{id}/members"),
        None,
    )
    .await;
    assert_eq!(members[0]["member_id"], bob_id, "{members}");
    assert_eq!(members[0]["username"], "bob", "{members}");

    let off = with(
        &k,
        SignupPolicy {
            disable_workspace_creation: true,
            ..SignupPolicy::default()
        },
    );
    let (_, auth) = call(&off, None, "GET", "/api/v1/auth", None).await;
    assert_eq!(auth["workspace_creation_disabled"], true);
    let body = Some(json!({ "name": "Another" }));
    let (status, _) = call(&off, Some(&bob), "POST", "/api/v1/workspaces", body.clone()).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, _) = call(&off, Some(TOKEN), "POST", "/api/v1/workspaces", body).await;
    assert_eq!(status, StatusCode::CREATED, "the token holder may");
}

/// The link token in the last mail.
pub(crate) fn link_token(link: &str) -> String {
    link.split("#/invite/").nth(1).unwrap().trim().to_owned()
}

#[tokio::test]
async fn tenancy_invitations_from_create_to_accept_decline_and_expiry() {
    let k = kernel().await;
    let app = &k.app;
    let (ada, _) = member(&k.store, "ada", &[]).await;
    let w = workspace(app, &ada, "Ops").await;
    let (mo, _) = member(&k.store, "mo", &[]).await;
    let (al, _) = member(&k.store, "al", &[]).await;
    let at = |what: &str| format!("/api/v1/workspaces/{w}/{what}");
    // An admin there invites up to admin; a member doesn't invite at all;
    // anyone else doesn't see the workspace.
    let mut tx = k.store.begin().await.unwrap();
    let mo_id = tx.member_by_email("mo@corp.io").await.unwrap().unwrap().id;
    let al_id = tx.member_by_email("al@corp.io").await.unwrap().unwrap().id;
    let w_id: WorkspaceId = w.parse().unwrap();
    tx.add_member(w_id, mo_id, Role::Member).await.unwrap();
    tx.add_member(w_id, al_id, Role::Admin).await.unwrap();
    tx.commit().await.unwrap();
    let (outsider, _) = member(&k.store, "out", &[]).await;
    let invite = |email: &str, role: &str| Some(json!({ "email": email, "role": role }));
    let (status, _) = call(
        app,
        Some(&mo),
        "POST",
        &at("invitations"),
        invite("x@corp.io", "viewer"),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "a member");
    let (status, _) = call(
        app,
        Some(&al),
        "POST",
        &at("invitations"),
        invite("x@corp.io", "owner"),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "an admin grants no owner");
    let (status, _) = call(
        app,
        Some(&outsider),
        "POST",
        &at("invitations"),
        invite("x@corp.io", "viewer"),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "not a member");
    let (status, value) = call(
        app,
        Some(&al),
        "POST",
        &at("invitations"),
        invite("mo@corp.io", "viewer"),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "already a member: {value}"
    );

    // No mail server: the admin gets the link to pass on.
    let (status, sent) = call(
        app,
        Some(&al),
        "POST",
        &at("invitations"),
        invite("Cat@Corp.io", "member"),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{sent}");
    assert_eq!(sent["mailed"], false);
    let link = sent["link"].as_str().unwrap();
    assert!(link.starts_with("http://localhost/#/invite/"), "{link}");
    let token = link_token(link);
    assert_eq!(token.len(), 43);
    let (_, pending) = call(app, Some(&al), "GET", &at("invitations"), None).await;
    assert_eq!(pending[0]["email"], "cat@corp.io", "{pending}");
    assert!(
        !pending.to_string().contains(&token),
        "the token is never stored or shown"
    );
    let (status, _) = call(app, Some(&mo), "GET", &at("invitations"), None).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    // The link's page, signed out.
    let (status, page) = call(
        app,
        None,
        "GET",
        &format!("/api/v1/invitations/{token}"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{page}");
    assert_eq!(page["workspace_name"], "Ops");
    assert_eq!(page["role"], "member");
    assert_eq!(page["inviter"], "al");
    assert_eq!(page["email"], "cat@corp.io");
    let (status, _) = call(app, None, "GET", "/api/v1/invitations/not-a-token", None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // With mail: the link goes by mail, not in the answer.
    smtp_on(app).await;
    let (status, sent) = call(
        app,
        Some(&al),
        "POST",
        &at("invitations"),
        invite("dan@corp.io", "viewer"),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{sent}");
    assert_eq!(
        (&sent["mailed"], &sent["link"]),
        (&json!(true), &Value::Null)
    );
    let mail = k.mails.last();
    assert_eq!(mail.to, "dan@corp.io");
    let dan_token = link_token(mail.body.lines().find(|l| l.contains("#/invite/")).unwrap());

    // Cat signs up; someone else can't take her invitation; she accepts once.
    let cat = sign_up(&k, app, "cat@corp.io").await;
    let (_, mine) = call(app, Some(&cat), "GET", "/api/v1/me/invitations", None).await;
    assert_eq!(mine[0]["workspace_name"], "Ops", "{mine}");
    let invitation = page["id"].as_str().unwrap().to_owned();
    let accept = format!("/api/v1/invitations/{invitation}/accept");
    let (status, _) = call(app, Some(&mo), "POST", &accept, None).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "another email");
    let (status, joined) = call(app, Some(&cat), "POST", &accept, None).await;
    assert_eq!(status, StatusCode::OK, "{joined}");
    assert_eq!(joined["role"], "member");
    let (status, _) = call(app, Some(&cat), "POST", &accept, None).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "used");
    let (status, _) = call(
        app,
        None,
        "GET",
        &format!("/api/v1/invitations/{token}"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "used");
    let (status, _) = call(app, Some(&cat), "GET", &at("members"), None).await;
    assert_eq!(status, StatusCode::OK, "she sees Ops now");

    // Accepting never lowers a role: dan became admin before accepting viewer.
    let dan = sign_up(&k, app, "dan@corp.io").await;
    let mut tx = k.store.begin().await.unwrap();
    let dan_id = tx.member_by_email("dan@corp.io").await.unwrap().unwrap().id;
    tx.add_member(w_id, dan_id, Role::Admin).await.unwrap();
    tx.commit().await.unwrap();
    let (_, page) = call(
        app,
        None,
        "GET",
        &format!("/api/v1/invitations/{dan_token}"),
        None,
    )
    .await;
    let accept = format!(
        "/api/v1/invitations/{}/accept",
        page["id"].as_str().unwrap()
    );
    let (status, joined) = call(app, Some(&dan), "POST", &accept, None).await;
    assert_eq!(
        (status, &joined["role"]),
        (StatusCode::OK, &json!("admin")),
        "{joined}"
    );

    // Declined, revoked (only through its own workspace) and expired: 404.
    let (_, sent) = call(
        app,
        Some(&al),
        "POST",
        &at("invitations"),
        invite("eve@corp.io", "viewer"),
    )
    .await;
    let eve_invitation = sent["invitation"]["id"].as_str().unwrap().to_owned();
    let eve = sign_up(&k, app, "eve@corp.io").await;
    let decline = format!("/api/v1/invitations/{eve_invitation}/decline");
    let (status, _) = call(app, Some(&eve), "POST", &decline, None).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (status, _) = call(app, Some(&eve), "POST", &decline, None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (_, sent) = call(
        app,
        Some(&al),
        "POST",
        &at("invitations"),
        invite("fay@corp.io", "viewer"),
    )
    .await;
    let fay_invitation = sent["invitation"]["id"].as_str().unwrap().to_owned();
    let other = workspace(app, &ada, "Other").await;
    let wrong = format!("/api/v1/workspaces/{other}/invitations/{fay_invitation}");
    let (status, _) = call(app, Some(&ada), "DELETE", &wrong, None).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "not this workspace's");
    let revoke = format!("{}/{fay_invitation}", at("invitations"));
    let (status, _) = call(app, Some(&al), "DELETE", &revoke, None).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (_, pending) = call(app, Some(&al), "GET", &at("invitations"), None).await;
    assert!(!pending.to_string().contains("fay@corp.io"), "{pending}");

    let expired_token = "e".repeat(43);
    let expired = WorkspaceInvitation {
        id: InvitationId::generate(),
        workspace_id: w_id,
        email: "eve@corp.io".into(),
        role: Role::Viewer,
        inviter_id: None,
        created_at: chrono::Utc::now() - chrono::Duration::days(8),
        expires_at: chrono::Utc::now() - chrono::Duration::days(1),
        accepted_at: None,
        declined_at: None,
    };
    let hash: String = Sha256::digest(expired_token.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    let mut tx = k.store.begin().await.unwrap();
    tx.insert_invitation(&expired, &hash).await.unwrap();
    tx.commit().await.unwrap();
    let (status, _) = call(
        app,
        None,
        "GET",
        &format!("/api/v1/invitations/{expired_token}"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "expired");
    let accept = format!("/api/v1/invitations/{}/accept", expired.id);
    let (status, _) = call(app, Some(&eve), "POST", &accept, None).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "expired");
}

#[tokio::test]
async fn tenancy_members_roles_last_owner_and_leaving() {
    let k = kernel().await;
    let app = &k.app;
    let (olga, olga_id) = member(&k.store, "olga", &[]).await;
    let w = workspace(app, &olga, "Ops").await;
    let w_id: WorkspaceId = w.parse().unwrap();
    let (al, al_id) = member(&k.store, "al", &[(w_id, Role::Admin)]).await;
    let (mo, mo_id) = member(&k.store, "mo", &[(w_id, Role::Member)]).await;
    let (_, vi_id) = member(&k.store, "vi", &[(w_id, Role::Viewer)]).await;
    let role = |who: &str| format!("/api/v1/workspaces/{w}/members/{who}");
    let to = |r: &str| Some(json!({ "role": r }));

    // Any member lists them; a member changes no role.
    let (status, listed) = call(
        app,
        Some(&mo),
        "GET",
        &format!("/api/v1/workspaces/{w}/members"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(listed.as_array().unwrap().len(), 4, "{listed}");
    let (status, _) = call(app, Some(&mo), "PUT", &role(&vi_id), to("member")).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    // An admin moves people up to admin, never touches or makes an owner.
    let (status, value) = call(app, Some(&al), "PUT", &role(&vi_id), to("admin")).await;
    assert_eq!(
        (status, &value["role"]),
        (StatusCode::OK, &json!("admin")),
        "{value}"
    );
    let (status, _) = call(app, Some(&al), "PUT", &role(&mo_id), to("owner")).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, _) = call(app, Some(&al), "PUT", &role(&olga_id), to("viewer")).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, _) = call(app, Some(&al), "DELETE", &role(&olga_id), None).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, _) = call(
        app,
        Some(&al),
        "PUT",
        &role("01J00000000000000000000000"),
        to("viewer"),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "not a member");
    // The last owner stays: 409 for demoting, removing or leaving.
    let (status, value) = call(app, Some(&olga), "PUT", &role(&olga_id), to("admin")).await;
    assert_eq!(status, StatusCode::CONFLICT, "{value}");
    let (status, _) = call(app, Some(&olga), "DELETE", &role(&olga_id), None).await;
    assert_eq!(status, StatusCode::CONFLICT);
    let leave = format!("/api/v1/workspaces/{w}/leave");
    let (status, _) = call(app, Some(&olga), "POST", &leave, None).await;
    assert_eq!(status, StatusCode::CONFLICT);
    // An owner makes another owner; then she may leave.
    let (status, _) = call(app, Some(&olga), "PUT", &role(&al_id), to("owner")).await;
    assert_eq!(status, StatusCode::OK);
    let (status, _) = call(app, Some(&olga), "POST", &leave, None).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (status, _) = call(
        app,
        Some(&olga),
        "GET",
        &format!("/api/v1/workspaces/{w}/members"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "gone from Ops");
    // An admin removes a member; they lose the workspace at once.
    let (status, _) = call(app, Some(&al), "DELETE", &role(&mo_id), None).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (status, _) = call(
        app,
        Some(&mo),
        "GET",
        &format!("/api/v1/workspaces/{w}/members"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (_, audit) = call(app, Some(TOKEN), "GET", "/api/v1/audit", None).await;
    assert!(audit.to_string().contains("mo in Ops"), "{audit}");
}

/// A one-step approval workflow named `name`.
fn flow(name: &str) -> String {
    format!("name: {name}\nsteps:\n  - id: ok\n    type: approval\n    who: [role:admin]\n")
}

#[tokio::test]
async fn tenancy_every_workspace_route_is_404_for_non_members() {
    let k = kernel().await;
    let app = &k.app;
    let w = workspace(app, TOKEN, "Hidden").await;
    let body = json!({ "source_yaml": flow("w"), "workspace_id": w });
    let workflow = as_token(app, "POST", "/api/v1/workflows", Some(body)).await["id"].clone();
    let workflow = workflow.as_str().unwrap().to_owned();
    let task = as_token(
        app,
        "POST",
        "/api/v1/tasks",
        Some(json!({ "title": "t", "workspace_id": w })),
    )
    .await;
    let task = task["id"].as_str().unwrap().to_owned();
    let project = as_token(
        app,
        "POST",
        &format!("/api/v1/workspaces/{w}/projects"),
        Some(json!({ "name": "p" })),
    )
    .await;
    let project = project["id"].as_str().unwrap().to_owned();
    let repo = as_token(
        app,
        "POST",
        &format!("/api/v1/workspaces/{w}/repos"),
        Some(json!({ "url": "https://example.com/r" })),
    )
    .await;
    let repo = repo["repo"]["id"].as_str().unwrap().to_owned();
    let source = as_token(
        app,
        "POST",
        "/api/v1/knowledge/sources",
        Some(json!({ "kind": "upload", "name": "s", "workspace_id": w })),
    )
    .await;
    let source = source["id"].as_str().unwrap().to_owned();
    let invitation = as_token(
        app,
        "POST",
        &format!("/api/v1/workspaces/{w}/invitations"),
        Some(json!({ "email": "z@corp.io", "role": "viewer" })),
    )
    .await;
    let invitation = invitation["invitation"]["id"].as_str().unwrap().to_owned();
    let (_, someone) = member(&k.store, "someone", &[(w.parse().unwrap(), Role::Viewer)]).await;
    // An admin and owner elsewhere (Main and their own workspace).
    let (stranger, _) = member(&k.store, "stranger", &[(MAIN, Role::Admin)]).await;
    workspace(app, &stranger, "Mine").await;

    let ws = format!("/api/v1/workspaces/{w}");
    let routes: Vec<(&str, String, Option<Value>)> = vec![
        ("PATCH", ws.clone(), Some(json!({ "name": "Taken" }))),
        ("DELETE", ws.clone(), None),
        ("GET", format!("{ws}/logo"), None),
        ("DELETE", format!("{ws}/logo"), None),
        (
            "POST",
            format!("{ws}/prefix"),
            Some(json!({ "issue_prefix": "X" })),
        ),
        ("GET", format!("{ws}/members"), None),
        (
            "PUT",
            format!("{ws}/members/{someone}"),
            Some(json!({ "role": "admin" })),
        ),
        ("DELETE", format!("{ws}/members/{someone}"), None),
        ("POST", format!("{ws}/leave"), None),
        ("GET", format!("{ws}/invitations"), None),
        (
            "POST",
            format!("{ws}/invitations"),
            Some(json!({ "email": "a@b.io", "role": "viewer" })),
        ),
        ("DELETE", format!("{ws}/invitations/{invitation}"), None),
        ("GET", format!("{ws}/projects"), None),
        (
            "POST",
            format!("{ws}/projects"),
            Some(json!({ "name": "q" })),
        ),
        (
            "PATCH",
            format!("/api/v1/projects/{project}"),
            Some(json!({ "name": "q" })),
        ),
        ("GET", format!("{ws}/repos"), None),
        (
            "POST",
            format!("{ws}/repos"),
            Some(json!({ "url": "https://example.com/s" })),
        ),
        (
            "PATCH",
            format!("/api/v1/repos/{repo}"),
            Some(json!({ "description": "d" })),
        ),
        ("DELETE", format!("{ws}/repos/{repo}"), None),
        ("GET", format!("{ws}/vcs"), None),
        ("GET", format!("{ws}/vcs/github/connect"), None),
        ("POST", format!("{ws}/knowledge_sources/{source}"), None),
        ("GET", format!("/api/v1/tasks?workspace={w}"), None),
        (
            "POST",
            "/api/v1/tasks".into(),
            Some(json!({ "title": "t", "workspace_id": w })),
        ),
        (
            "POST",
            "/api/v1/tasks".into(),
            Some(json!({ "title": "t", "workflow_id": workflow })),
        ),
        ("GET", format!("/api/v1/tasks/{task}"), None),
        (
            "PATCH",
            format!("/api/v1/tasks/{task}"),
            Some(json!({ "title": "u" })),
        ),
        (
            "POST",
            format!("/api/v1/tasks/{task}/comments"),
            Some(json!({ "body": "hi" })),
        ),
        ("POST", format!("/api/v1/tasks/{task}/cancel"), None),
        ("POST", format!("/api/v1/tasks/{task}/approve"), None),
        ("GET", format!("/api/v1/tasks/{task}/activity"), None),
        (
            "PUT",
            format!("/api/v1/tasks/{task}/budget"),
            Some(json!({ "cost_usd": 1.0 })),
        ),
        ("GET", format!("/api/v1/workflows?workspace={w}"), None),
        ("GET", format!("/api/v1/workflows/{workflow}"), None),
        (
            "POST",
            format!("/api/v1/workflows/{workflow}/versions"),
            Some(json!({ "source_yaml": flow("w") })),
        ),
        (
            "POST",
            "/api/v1/workflows".into(),
            Some(json!({ "source_yaml": flow("v"), "workspace_id": w })),
        ),
        (
            "POST",
            format!("/api/v1/templates/ship/install?workspace={w}"),
            None,
        ),
        (
            "GET",
            format!("/api/v1/knowledge/sources?workspace={w}"),
            None,
        ),
        (
            "GET",
            format!("/api/v1/knowledge/sources/{source}/docs"),
            None,
        ),
        (
            "DELETE",
            format!("/api/v1/knowledge/sources/{source}"),
            None,
        ),
        (
            "GET",
            format!("/api/v1/knowledge/search?q=x&workspace={w}"),
            None,
        ),
    ];
    for (method, path, body) in &routes {
        let (status, value) = call(app, Some(&stranger), method, path, body.clone()).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{method} {path}: {value}");
    }
    let (_, listed) = call(app, Some(&stranger), "GET", "/api/v1/workspaces", None).await;
    assert!(!listed.to_string().contains("Hidden"), "{listed}");
}

#[tokio::test]
async fn tenancy_the_role_in_each_workspace_decides() {
    let k = kernel().await;
    let app = &k.app;
    // Bea is a viewer in Main and the owner of B.
    let (bea, _) = member(&k.store, "bea", &[(MAIN, Role::Viewer)]).await;
    let b = workspace(app, &bea, "B").await;
    let main = MAIN.to_string();
    for (workspace, want_workflow, want_rename) in [
        (b.as_str(), StatusCode::CREATED, StatusCode::OK),
        (main.as_str(), StatusCode::FORBIDDEN, StatusCode::FORBIDDEN),
    ] {
        let body = Some(json!({ "source_yaml": flow("mine"), "workspace_id": workspace }));
        let (status, value) = call(app, Some(&bea), "POST", "/api/v1/workflows", body).await;
        assert_eq!(status, want_workflow, "{workspace}: {value}");
        let body = Some(json!({ "title": "t", "workspace_id": workspace }));
        let (status, _) = call(app, Some(&bea), "POST", "/api/v1/tasks", body).await;
        let want_task = match want_workflow {
            StatusCode::CREATED => StatusCode::CREATED,
            _ => StatusCode::FORBIDDEN,
        };
        assert_eq!(status, want_task, "{workspace}: new task");
        let rename = Some(json!({ "name": if workspace == b { "B2" } else { "Main" } }));
        let (status, _) = call(
            app,
            Some(&bea),
            "PATCH",
            &format!("/api/v1/workspaces/{workspace}"),
            rename,
        )
        .await;
        assert_eq!(status, want_rename, "{workspace}: rename");
    }
    // B's runs need a host there: she didn't enrol the kernel's (404), a
    // deployment owner attaches it.
    let host = format!("/api/v1/workspaces/{b}/hosts/{}", k.host);
    let (status, _) = call(app, Some(&bea), "PUT", &host, None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _) = call(app, Some(TOKEN), "PUT", &host, None).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    // A task in B runs and waits for `role:admin`: as B's owner she approves.
    let body = json!({ "source_yaml": flow("gate"), "workspace_id": b });
    let (_, workflow) = call(app, Some(&bea), "POST", "/api/v1/workflows", Some(body)).await;
    let body = Some(json!({ "title": "t", "workflow_id": workflow["id"] }));
    let (_, task) = call(app, Some(&bea), "POST", "/api/v1/tasks", body).await;
    let task = task["id"].as_str().unwrap().to_owned();
    let approve = format!("/api/v1/tasks/{task}/approve");
    let mut approved = StatusCode::CONFLICT;
    for _ in 0..250 {
        approved = call(app, Some(&bea), "POST", &approve, None).await.0;
        if approved != StatusCode::CONFLICT {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(approved, StatusCode::OK);
}

#[tokio::test]
async fn tenancy_deployment_routes_are_for_deployment_owners_only() {
    let k = kernel().await;
    let app = &k.app;
    // Admin of Main and owner of their own workspace, but no deployment owner.
    let (ada, ada_id) = member(&k.store, "ada", &[(MAIN, Role::Admin)]).await;
    let mine = workspace(app, &ada, "Mine").await;
    let routes: Vec<(&str, &str, Option<Value>)> = vec![
        ("GET", "/members", None),
        (
            "POST",
            "/members",
            Some(json!({ "email": "a@b.io", "username": "a" })),
        ),
        ("POST", "/members/x/deactivate", None),
        ("POST", "/members/x/activate", None),
        (
            "PUT",
            "/members/x/password",
            Some(json!({ "password": "p" })),
        ),
        ("PUT", "/members/x/owner", Some(json!({ "is_owner": true }))),
        ("POST", "/members/x/invite", None),
        ("DELETE", "/members/x/two_step", None),
        ("GET", "/smtp", None),
        ("PUT", "/smtp", Some(json!({ "host": "" }))),
        ("POST", "/smtp/test", None),
        ("GET", "/settings/sso", None),
        ("PUT", "/settings/sso/x", Some(json!({}))),
        ("GET", "/settings/security", None),
        ("PUT", "/settings/security", Some(json!({}))),
        ("GET", "/audit", None),
        ("GET", "/kernel", None),
        (
            "POST",
            "/mods/x/install",
            Some(json!({ "permissions": [] })),
        ),
        ("DELETE", "/mods/x", None),
        (
            "PUT",
            "/secrets/ecosystem/x/NAME",
            Some(json!({ "value": "v" })),
        ),
        (
            "PUT",
            "/settings/on_limit",
            Some(json!({ "on_limit": null })),
        ),
        (
            "PUT",
            "/settings/budget",
            Some(json!({ "monthly_usd": null })),
        ),
        ("PUT", "/settings/embeddings", Some(json!({}))),
        ("PUT", "/catalog/tdd/dismissed", None),
        (
            "PUT",
            "/runtimes/claude-code/policy",
            Some(json!({ "permission_mode": "default" })),
        ),
    ];
    for (method, path, body) in &routes {
        // Naming their own workspace (query and body) opens nothing.
        let mut body = body.clone();
        if let Some(Value::Object(map)) = body.as_mut() {
            map.insert("workspace_id".into(), json!(mine));
        }
        let path = format!("/api/v1{path}?workspace={mine}");
        let (status, value) = call(app, Some(&ada), method, &path, body).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{method} {path}: {value}");
    }
    let (status, _) = call(app, Some(TOKEN), "GET", "/api/v1/members", None).await;
    assert_eq!(status, StatusCode::OK);
    // Promoting her opens them.
    let path = format!("/api/v1/members/{ada_id}/owner");
    as_token(app, "PUT", &path, Some(json!({ "is_owner": true }))).await;
    let (status, _) = call(app, Some(&ada), "GET", "/api/v1/audit", None).await;
    assert_eq!(status, StatusCode::OK);
}

/// Agents, runtimes, secrets and memory are each workspace's own: members
/// read them there (`?workspace=`, required for anyone but a deployment
/// owner), another workspace's answer 404, and writes need the role there.
#[tokio::test]
async fn tenancy_agents_runtimes_secrets_and_memory_are_per_workspace() {
    use sun_core::model::{MemoryItem, MemoryScope, MemoryStatus};
    use sun_store::MemoryRepo;

    let k = kernel().await;
    let app = &k.app;
    let (ada, _) = member(&k.store, "ada", &[]).await;
    let (bob, _) = member(&k.store, "bob", &[]).await;
    let a = workspace(app, &ada, "Alpha").await;
    let b = workspace(app, &bob, "Beta").await;
    let a_id: WorkspaceId = a.parse().unwrap();
    let (vic, _) = member(&k.store, "vic", &[(a_id, Role::Viewer)]).await;
    let ok = |status: StatusCode, value: &Value| assert!(status.is_success(), "{status} {value}");

    // Agents: one name per workspace; each lists only its own.
    let agent = json!({ "name": "Ada", "runtime_chain": ["claude-code"] });
    let path = |w: &str| format!("/api/v1/agents?workspace={w}");
    let (status, mine) = call(app, Some(&ada), "POST", &path(&a), Some(agent.clone())).await;
    ok(status, &mine);
    assert_eq!(mine["workspace_id"], json!(a));
    let (status, theirs) = call(app, Some(&bob), "POST", &path(&b), Some(agent)).await;
    ok(status, &theirs);
    let (status, listed) = call(app, Some(&ada), "GET", &path(&a), None).await;
    ok(status, &listed);
    assert_eq!(listed.as_array().unwrap().len(), 1);
    assert_eq!(listed[0]["id"], mine["id"]);
    let (status, _) = call(app, Some(&ada), "GET", &path(&b), None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _) = call(app, Some(&ada), "GET", "/api/v1/agents", None).await;
    assert_eq!(
        status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "a member names the workspace"
    );
    let theirs_path = format!("/api/v1/agents/{}", theirs["id"].as_str().unwrap());
    for method in ["PATCH", "DELETE"] {
        let body = (method == "PATCH").then(|| json!({ "name": "Mine" }));
        let (status, _) = call(app, Some(&ada), method, &theirs_path, body).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{method}");
    }
    // A viewer reads; writing needs `manage_runtimes` there.
    let (status, listed) = call(app, Some(&vic), "GET", &path(&a), None).await;
    ok(status, &listed);
    assert_eq!(listed.as_array().unwrap().len(), 1);
    let new = json!({ "name": "Vic", "runtime_chain": ["codex"] });
    let (status, _) = call(app, Some(&vic), "POST", &path(&a), Some(new)).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let mine_path = format!("/api/v1/agents/{}", mine["id"].as_str().unwrap());
    let (status, _) = call(app, Some(&vic), "DELETE", &mine_path, None).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    // An agent of another workspace is no assignee here.
    let task = json!({ "title": "x", "workspace_id": a, "assignee": { "kind": "agent", "id": theirs["id"] } });
    let (status, _) = call(app, Some(&ada), "POST", "/api/v1/tasks", Some(task)).await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // Runtimes: a BYO key runtime is Alpha's; its key is Alpha's secret.
    let runtime = json!({
        "name": "ollama", "source": "byo_key",
        "config": { "endpoint": "http://localhost:11434/v1", "model": "qwen", "key_secret": "OR_KEY" },
    });
    let path = |w: &str| format!("/api/v1/runtimes?workspace={w}");
    let (status, saved) = call(app, Some(&ada), "POST", &path(&a), Some(runtime.clone())).await;
    ok(status, &saved);
    assert_eq!(saved["workspace_id"], json!(a));
    let (status, _) = call(app, Some(&vic), "POST", &path(&a), Some(runtime.clone())).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let stored = |list: &Value| -> Vec<String> {
        let rows = list.as_array().unwrap().iter();
        rows.filter(|r| !r["workspace_id"].is_null())
            .map(|r| r["name"].as_str().unwrap().to_owned())
            .collect()
    };
    let (_, listed) = call(app, Some(&vic), "GET", &path(&a), None).await;
    assert_eq!(stored(&listed), ["ollama"]);
    let (_, listed) = call(app, Some(&bob), "GET", &path(&b), None).await;
    assert!(stored(&listed).is_empty(), "{listed}");
    let (status, _) = call(app, Some(&bob), "GET", &path(&a), None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let in_b = format!("/api/v1/runtimes/ollama?workspace={b}");
    let (status, _) = call(app, Some(&bob), "PUT", &in_b, Some(runtime.clone())).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "Beta has no `ollama`");
    let readiness = format!("/api/v1/runtimes/ollama/readiness?workspace={a}");
    let (_, ready) = call(app, Some(&vic), "GET", &readiness, None).await;
    assert_eq!(ready["logged_in"], false, "{ready}");

    // Secrets: Alpha's, by `manage_secrets` there.
    let secret = |w: &str, name: &str| format!("/api/v1/secrets/workspace/{w}/{name}");
    let value = Some(json!({ "value": "sk-alpha-0123456789" }));
    let (status, set) = call(app, Some(&bob), "PUT", &secret(&b, "OR_KEY"), value.clone()).await;
    ok(status, &set);
    let (_, ready) = call(app, Some(&vic), "GET", &readiness, None).await;
    assert_eq!(ready["logged_in"], false, "Beta's key is not Alpha's");
    let (status, set) = call(app, Some(&ada), "PUT", &secret(&a, "OR_KEY"), value.clone()).await;
    ok(status, &set);
    let (_, ready) = call(app, Some(&vic), "GET", &readiness, None).await;
    assert_eq!(ready["logged_in"], true, "{ready}");
    let listing = format!("/api/v1/secrets?workspace={a}");
    let (status, names) = call(app, Some(&ada), "GET", &listing, None).await;
    ok(status, &names);
    assert_eq!(names.as_array().unwrap().len(), 1);
    assert_eq!(names[0]["name"], "OR_KEY");
    assert!(!names.to_string().contains("sk-alpha"), "never a value");
    for (token, method, path, want) in [
        (&bob, "GET", listing.clone(), StatusCode::NOT_FOUND),
        (&bob, "PUT", secret(&a, "X_KEY"), StatusCode::NOT_FOUND),
        (&bob, "DELETE", secret(&a, "OR_KEY"), StatusCode::NOT_FOUND),
        (&vic, "GET", listing.clone(), StatusCode::FORBIDDEN),
        (&vic, "PUT", secret(&a, "X_KEY"), StatusCode::FORBIDDEN),
        (
            &ada,
            "PUT",
            secret(&a, "VCS_X_TOKEN"),
            StatusCode::UNPROCESSABLE_ENTITY,
        ),
    ] {
        let body = (method == "PUT").then(|| json!({ "value": "sk-other-0123456789" }));
        let (status, value) = call(app, Some(token), method, &path, body).await;
        assert_eq!(status, want, "{method} {path}: {value}");
    }
    // The ecosystem scope keeps the deployment's own names only.
    let eco = |name: &str| format!("/api/v1/secrets/ecosystem/00000000000000000000000000/{name}");
    let (status, _) = call(app, Some(TOKEN), "PUT", &eco("OR_KEY"), value.clone()).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    let (status, _) = call(app, Some(TOKEN), "PUT", &eco("GITHUB_APP_SLUG"), value).await;
    assert_eq!(status, StatusCode::OK);

    // Memory: each workspace's; reviews need `manage_settings` there.
    let item = MemoryItem {
        id: sun_core::MemoryItemId::generate(),
        workspace_id: b.parse().unwrap(),
        scope: MemoryScope::Ecosystem,
        scope_id: None,
        body: "Run the tests.".into(),
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
    let memory = |w: &str| format!("/api/v1/memory?workspace={w}");
    let (_, alpha) = call(app, Some(&ada), "GET", &memory(&a), None).await;
    assert_eq!(alpha, json!([]));
    let (_, beta) = call(app, Some(&bob), "GET", &memory(&b), None).await;
    assert_eq!(beta[0]["id"], json!(item.id));
    let (status, _) = call(app, Some(&ada), "GET", &memory(&b), None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let metrics = format!("/api/v1/memory/metrics?workspace={a}");
    let (status, _) = call(app, Some(&vic), "GET", &metrics, None).await;
    assert_eq!(status, StatusCode::OK);
    let review = format!("/api/v1/memory/{}/review", item.id);
    let active = Some(json!({ "status": "active" }));
    let (status, _) = call(app, Some(&ada), "POST", &review, active.clone()).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, reviewed) = call(app, Some(&bob), "POST", &review, active).await;
    ok(status, &reviewed);
    // A deployment owner sees every workspace's.
    let (_, all) = call(app, Some(TOKEN), "GET", "/api/v1/agents", None).await;
    assert_eq!(all.as_array().unwrap().len(), 2);
    let (_, all) = call(app, Some(TOKEN), "GET", "/api/v1/memory", None).await;
    assert_eq!(all.as_array().unwrap().len(), 1);
}

/// Hosts, catalog installs and MCP servers are each workspace's: a token
/// minted in a workspace enrols a host attached to it, members list only
/// their workspace's, another workspace's answer 404, and changes need
/// `manage_runtimes` (hosts) or `manage_plugins` (catalog, MCP) there.
#[tokio::test(flavor = "multi_thread")]
async fn tenancy_hosts_catalog_and_mcp_servers_are_per_workspace() {
    let k = kernel().await;
    let app = &k.app;
    let (ada, ada_id) = member(&k.store, "ada", &[]).await;
    let (bob, _) = member(&k.store, "bob", &[]).await;
    let a = workspace(app, &ada, "Alpha").await;
    let b = workspace(app, &bob, "Beta").await;
    let (vic, _) = member(&k.store, "vic", &[(a.parse().unwrap(), Role::Viewer)]).await;

    // An enrolment token is minted in Alpha, by `manage_runtimes` there.
    let tokens = |w: &str| format!("/api/v1/workspaces/{w}/hosts/tokens");
    let (status, _) = call(app, Some(&vic), "POST", &tokens(&a), None).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, _) = call(app, Some(&bob), "POST", &tokens(&a), None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, issued) = call(app, Some(&ada), "POST", &tokens(&a), None).await;
    assert_eq!(status, StatusCode::CREATED, "{issued}");
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("ws://{}/host", listener.local_addr().unwrap());
    tokio::spawn(axum::serve(listener, app.clone()).into_future());
    let dir = tempfile::tempdir().unwrap();
    let token = issued["token"].as_str().unwrap();
    let joined = sun_host::enroll(&url, token, "ada-mac", dir.path())
        .await
        .unwrap();
    let host = joined.host_id.to_string();

    // Alpha lists it (with who enrolled it); Beta doesn't, nor the kernel's.
    let hosts = |w: &str| format!("/api/v1/hosts?workspace={w}");
    let ids = |listed: &Value| -> Vec<String> {
        let listed = listed.as_array().unwrap().iter();
        listed
            .map(|h| h["id"].as_str().unwrap().to_owned())
            .collect()
    };
    let (status, listed) = call(app, Some(&vic), "GET", &hosts(&a), None).await;
    assert_eq!(status, StatusCode::OK, "{listed}");
    assert_eq!(ids(&listed), std::slice::from_ref(&host));
    assert_eq!(listed[0]["workspace_ids"], json!([a]));
    assert_eq!(listed[0]["enrolled_by"], json!(ada_id));
    let (_, listed) = call(app, Some(&bob), "GET", &hosts(&b), None).await;
    assert_eq!(listed, json!([]));
    let (status, _) = call(app, Some(&bob), "GET", &hosts(&a), None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _) = call(app, Some(&bob), "GET", "/api/v1/hosts", None).await;
    assert_eq!(
        status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "a member names one"
    );
    let (_, every) = call(app, Some(TOKEN), "GET", "/api/v1/hosts", None).await;
    assert_eq!(every.as_array().unwrap().len(), 2, "owners see every host");

    // Attach: who enrolled it, to another workspace of theirs; not Bob, who
    // didn't, and not into a workspace Ada isn't in.
    let attach = |w: &str| format!("/api/v1/workspaces/{w}/hosts/{host}");
    let (status, _) = call(app, Some(&bob), "PUT", &attach(&b), None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _) = call(app, Some(&ada), "PUT", &attach(&b), None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let kernel_host = format!("/api/v1/workspaces/{a}/hosts/{}", k.host);
    let (status, _) = call(app, Some(&ada), "PUT", &kernel_host, None).await;
    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "she didn't enrol the kernel's"
    );
    let g = workspace(app, &ada, "Gamma").await;
    let (status, _) = call(app, Some(&ada), "PUT", &attach(&g), None).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (_, listed) = call(app, Some(&ada), "GET", &hosts(&a), None).await;
    let mut both = [a.clone(), g.clone()];
    both.sort();
    assert_eq!(listed[0]["workspace_ids"], json!(both));
    let (_, listed) = call(app, Some(&vic), "GET", &hosts(&a), None).await;
    assert_eq!(
        listed[0]["workspace_ids"],
        json!([a]),
        "only the ones vic sees"
    );
    let (status, _) = call(app, Some(&vic), "DELETE", &attach(&a), None).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, _) = call(app, Some(&ada), "DELETE", &attach(&g), None).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (status, _) = call(app, Some(&ada), "DELETE", &attach(&g), None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // Catalog installs: Alpha's own; the files stay while Beta has it.
    let install = |w: &str| format!("/api/v1/catalog/tdd/install?workspace={w}");
    let uninstall = |w: &str| format!("/api/v1/catalog/tdd?workspace={w}");
    let catalog = |w: &str| format!("/api/v1/catalog?workspace={w}");
    let installed = |view: &Value| -> bool {
        let entries = view["entries"].as_array().unwrap();
        let tdd = entries.iter().find(|e| e["id"] == "tdd").unwrap();
        !tdd["installed"].is_null()
    };
    let (status, _) = call(app, Some(&vic), "POST", &install(&a), Some(json!({}))).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, _) = call(app, Some(&bob), "POST", &install(&a), Some(json!({}))).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _) = call(app, Some(&ada), "POST", &install(&a), Some(json!({}))).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (status, view) = call(app, Some(&vic), "GET", &catalog(&a), None).await;
    assert_eq!(status, StatusCode::OK, "{view}");
    assert!(installed(&view));
    let (_, view) = call(app, Some(&bob), "GET", &catalog(&b), None).await;
    assert!(!installed(&view), "Alpha's install isn't Beta's");
    let (status, _) = call(app, Some(&bob), "GET", &catalog(&a), None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let probe = format!("/api/v1/catalog?workspace={a}&repo=/tmp");
    let (status, _) = call(app, Some(&ada), "GET", &probe, None).await;
    assert_eq!(
        status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "no folder probes on the kernel's machine"
    );
    let (status, _) = call(app, Some(&bob), "POST", &install(&b), Some(json!({}))).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (status, _) = call(app, Some(&ada), "DELETE", &uninstall(&a), None).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (status, _) = call(app, Some(&ada), "DELETE", &uninstall(&a), None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (_, view) = call(app, Some(&bob), "GET", &catalog(&b), None).await;
    assert!(installed(&view), "Beta keeps its install");
    let (_, view) = call(app, Some(&ada), "GET", &catalog(&a), None).await;
    assert!(!installed(&view));

    // MCP servers: listed per workspace; registering needs `manage_plugins`.
    let servers = |w: &str| format!("/api/v1/mcp_servers?workspace={w}");
    let (status, listed) = call(app, Some(&vic), "GET", &servers(&a), None).await;
    assert_eq!((status, listed), (StatusCode::OK, json!([])));
    let (status, _) = call(app, Some(&bob), "GET", &servers(&a), None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let server =
        json!({"name": "docs", "transport": {"type": "url", "url": "https://x.example/mcp"}});
    let (status, _) = call(app, Some(&vic), "POST", &servers(&a), Some(server.clone())).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, _) = call(app, Some(&bob), "POST", &servers(&a), Some(server)).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}
