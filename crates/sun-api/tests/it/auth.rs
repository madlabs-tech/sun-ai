//! SSO sign-in (Google, GitHub and generic OIDC) against a mock provider,
//! roles on every route group, approval `who: role:x` matching, and the
//! audit log — through the real kernel.
#![allow(clippy::unwrap_used, clippy::expect_used)] // test helpers

use crate::common;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::http::{Request, StatusCode, header};
use axum::routing::{get, post};
use common::{TOKEN, kernel, kernel_with, restart};
use http_body_util::BodyExt;
use openidconnect::core::{
    CoreIdToken, CoreIdTokenClaims, CoreJsonWebKeySet, CoreJwsSigningAlgorithm,
    CoreProviderMetadata, CoreResponseType, CoreRsaPrivateSigningKey, CoreSubjectIdentifierType,
};
use openidconnect::url::{Url, form_urlencoded};
use openidconnect::{
    AccessToken, Audience, AuthUrl, EmptyAdditionalClaims, EmptyAdditionalProviderMetadata,
    EndUserEmail, EndUserUsername, IssuerUrl, JsonWebKeyId, JsonWebKeySetUrl, Nonce,
    PkceCodeChallenge, PkceCodeVerifier, PrivateSigningKey, ResponseTypes, StandardClaims,
    SubjectIdentifier, TokenUrl,
};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use sun_api::PresetUrls;
use sun_core::WorkspaceId;
use sun_core::model::{Role, SignupPolicy};
use sun_store::{LoginRepo, MemberRepo};

/// Throwaway key that only signs the mock provider's ID tokens.
const IDP_KEY: &str = include_str!("../fixtures/mock_idp_rsa.pem");
const CLIENT_ID: &str = "sun";

/// What the mock provider puts in the next ID token (or GitHub answer), and
/// the login it saw.
#[derive(Default)]
struct Login {
    nonce: String,
    challenge: String,
    subject: String,
    username: String,
    email: String,
    /// The provider didn't verify the email.
    unverified: bool,
}

#[derive(Clone)]
struct Idp {
    issuer: String,
    login: Arc<Mutex<Login>>,
}

fn signing_key() -> CoreRsaPrivateSigningKey {
    CoreRsaPrivateSigningKey::from_pem(IDP_KEY, Some(JsonWebKeyId::new("k1".into()))).unwrap()
}

/// An OIDC provider with discovery, JWKS and a token endpoint that checks
/// PKCE; it also plays GitHub (its OAuth2 token endpoint, `/user` and
/// `/user/emails`) and is Google's issuer through [`Idp::presets`].
async fn mock_idp() -> Idp {
    mock_idp_with("").await
}

/// [`mock_idp`] whose issuer ends with `suffix` (Auth0's ends with `/`).
async fn mock_idp_with(suffix: &str) -> Idp {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let issuer = format!("http://{}{suffix}", listener.local_addr().unwrap());
    let idp = Idp {
        issuer: issuer.clone(),
        login: Arc::default(),
    };
    let app = Router::new()
        .route("/.well-known/openid-configuration", get(discovery))
        .route("/jwks", get(|| async { axum::Json(jwks()) }))
        .route("/token", post(token))
        .route("/login/oauth/access_token", post(github_token))
        .route("/user", get(github_user))
        .route("/user/emails", get(github_emails))
        .with_state(idp.clone());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    idp
}

async fn discovery(State(idp): State<Idp>) -> axum::Json<CoreProviderMetadata> {
    // Endpoints never double the issuer's trailing slash.
    let issuer = idp.issuer.trim_end_matches('/');
    let metadata = CoreProviderMetadata::new(
        IssuerUrl::new(idp.issuer.clone()).unwrap(),
        AuthUrl::new(format!("{issuer}/authorize")).unwrap(),
        JsonWebKeySetUrl::new(format!("{issuer}/jwks")).unwrap(),
        vec![ResponseTypes::new(vec![CoreResponseType::Code])],
        vec![CoreSubjectIdentifierType::Public],
        vec![CoreJwsSigningAlgorithm::RsaSsaPkcs1V15Sha256],
        EmptyAdditionalProviderMetadata {},
    )
    .set_token_endpoint(Some(TokenUrl::new(format!("{issuer}/token")).unwrap()));
    axum::Json(metadata)
}

fn jwks() -> CoreJsonWebKeySet {
    CoreJsonWebKeySet::new(vec![signing_key().as_verification_key()])
}

async fn token(State(idp): State<Idp>, body: String) -> (StatusCode, axum::Json<Value>) {
    let form: Vec<(String, String)> = form_urlencoded::parse(body.as_bytes())
        .into_owned()
        .collect();
    let field = |name: &str| {
        form.iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.clone())
            .unwrap_or_default()
    };
    let login = idp.login.lock().unwrap();
    let verifier = PkceCodeVerifier::new(field("code_verifier"));
    let pkce_ok =
        PkceCodeChallenge::from_code_verifier_sha256(&verifier).as_str() == login.challenge;
    if field("code") != "good-code" || !pkce_ok {
        return (
            StatusCode::BAD_REQUEST,
            axum::Json(json!({ "error": "invalid_grant" })),
        );
    }
    let now = chrono::Utc::now();
    let claims = CoreIdTokenClaims::new(
        IssuerUrl::new(idp.issuer.clone()).unwrap(),
        vec![Audience::new(CLIENT_ID.into())],
        now + chrono::Duration::hours(1),
        now,
        StandardClaims::new(SubjectIdentifier::new(login.subject.clone()))
            .set_email(Some(EndUserEmail::new(login.email.clone())))
            .set_email_verified(Some(!login.unverified))
            .set_preferred_username(Some(EndUserUsername::new(login.username.clone()))),
        EmptyAdditionalClaims {},
    )
    .set_nonce(Some(Nonce::new(login.nonce.clone())));
    let access = AccessToken::new("access-1".into());
    let id_token = CoreIdToken::new(
        claims,
        &signing_key(),
        CoreJwsSigningAlgorithm::RsaSsaPkcs1V15Sha256,
        Some(&access),
        None,
    )
    .unwrap();
    (
        StatusCode::OK,
        axum::Json(json!({
            "access_token": access.secret(),
            "token_type": "bearer",
            "expires_in": 3600,
            "id_token": id_token.to_string(),
        })),
    )
}

/// GitHub's token endpoint: the client secret comes in the form body.
async fn github_token(State(idp): State<Idp>, body: String) -> (StatusCode, axum::Json<Value>) {
    let form: Vec<(String, String)> = form_urlencoded::parse(body.as_bytes())
        .into_owned()
        .collect();
    let field = |name: &str| {
        form.iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.clone())
            .unwrap_or_default()
    };
    let login = idp.login.lock().unwrap();
    let verifier = PkceCodeVerifier::new(field("code_verifier"));
    let pkce_ok =
        PkceCodeChallenge::from_code_verifier_sha256(&verifier).as_str() == login.challenge;
    let client_ok = field("client_id") == CLIENT_ID && field("client_secret") == "client-secret";
    if field("code") != "good-code" || !pkce_ok || !client_ok {
        // GitHub answers a bad code with 200 and an error body.
        return (
            StatusCode::OK,
            axum::Json(json!({ "error": "bad_verification_code" })),
        );
    }
    (
        StatusCode::OK,
        axum::Json(
            json!({ "access_token": "gho_1", "token_type": "bearer", "scope": "read:user,user:email" }),
        ),
    )
}

fn github_auth(headers: &HeaderMap) -> Result<(), StatusCode> {
    let ok = headers
        .get(header::AUTHORIZATION)
        .is_some_and(|v| v == "Bearer gho_1")
        && headers.contains_key(header::USER_AGENT);
    ok.then_some(()).ok_or(StatusCode::UNAUTHORIZED)
}

async fn github_user(
    State(idp): State<Idp>,
    headers: HeaderMap,
) -> Result<axum::Json<Value>, StatusCode> {
    github_auth(&headers)?;
    let login = idp.login.lock().unwrap();
    let id: u64 = login.subject.parse().unwrap();
    Ok(axum::Json(json!({ "id": id, "login": login.username })))
}

async fn github_emails(
    State(idp): State<Idp>,
    headers: HeaderMap,
) -> Result<axum::Json<Value>, StatusCode> {
    github_auth(&headers)?;
    let login = idp.login.lock().unwrap();
    Ok(axum::Json(json!([
        { "email": "someone-else@corp.io", "primary": false, "verified": true, "visibility": null },
        { "email": login.email, "primary": true, "verified": !login.unverified, "visibility": "private" },
    ])))
}

impl Idp {
    /// The Google and GitHub presets pointed at this mock.
    fn presets(&self) -> PresetUrls {
        PresetUrls {
            google: self.issuer.clone(),
            github_web: self.issuer.clone(),
            github_api: self.issuer.clone(),
        }
    }
}

/// Settings → Sign-in: adds provider `id` of `kind` at the mock, as the
/// token holder.
async fn add_provider(app: &Router, idp: &Idp, id: &str, kind: &str, allowed_domains: &[&str]) {
    let body = json!({
        "kind": kind,
        "name": "Mock IdP",
        "client_id": CLIENT_ID,
        "issuer_url": idp.issuer,
        "allowed_domains": allowed_domains,
        "client_secret": "client-secret",
    });
    let path = format!("/api/v1/settings/sso/{id}");
    let (status, _, body) = send(app, "PUT", &path, None, Some(body)).await;
    assert_eq!(status, StatusCode::OK, "{body}");
}

/// A kernel with the generic OIDC provider `mock` at `idp`.
async fn oidc_kernel(idp: &Idp, allowed_domains: &[&str]) -> common::Kernel {
    let k = kernel_with(Some(idp.presets())).await;
    add_provider(&k.app, idp, "mock", "oidc", allowed_domains).await;
    k
}

/// An admin invited `name@domain` as `role` in Main (straight in the store).
async fn invite(k: &common::Kernel, name: &str, domain: &str, role: Role) {
    use sun_store::MembershipRepo;
    let mut tx = k.store.begin().await.unwrap();
    let member = tx
        .invite_member(&format!("{name}@{domain}"), name, false)
        .await
        .unwrap();
    tx.add_member(WorkspaceId::MAIN, member.id, role)
        .await
        .unwrap();
    tx.commit().await.unwrap();
}

/// `k`'s API with sign-up closed: only invited people get in.
fn invite_only(k: &common::Kernel) -> Router {
    let closed = SignupPolicy {
        allow_signup: false,
        ..SignupPolicy::default()
    };
    sun_api::router(k.api.clone().with_signup_policy(closed))
}

/// A request as the token holder (`auth = None`) or with this cookie.
async fn send(
    app: &Router,
    method: &str,
    path: &str,
    cookie: Option<&str>,
    body: Option<Value>,
) -> (StatusCode, axum::http::HeaderMap, Value) {
    let mut request = Request::builder()
        .method(method)
        .uri(path)
        .header(header::CONTENT_TYPE, "application/json");
    request = match cookie {
        Some(cookie) => request.header(header::COOKIE, cookie),
        None => request.header(header::AUTHORIZATION, format!("Bearer {TOKEN}")),
    };
    let request = request
        .body(body.map_or(Body::empty(), |b| Body::from(b.to_string())))
        .unwrap();
    let response = common::respond(app, request).await;
    let (status, headers) = (response.status(), response.headers().clone());
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, headers, value)
}

fn location(headers: &axum::http::HeaderMap) -> String {
    headers[header::LOCATION].to_str().unwrap().to_owned()
}

/// `name=value` of the `Set-Cookie` for `name`.
fn set_cookie(headers: &axum::http::HeaderMap, name: &str) -> String {
    headers
        .get_all(header::SET_COOKIE)
        .iter()
        .map(|v| v.to_str().unwrap())
        .find(|v| v.starts_with(&format!("{name}=")))
        .and_then(|v| v.split(';').next())
        .unwrap()
        .to_owned()
}

/// Runs the browser side of a login as `username@domain` through provider
/// `mock`; returns where the callback sent the browser and the session
/// cookie, if any.
async fn sign_in(
    app: &Router,
    idp: &Idp,
    username: &str,
    domain: &str,
) -> (String, Option<String>) {
    sign_in_from(app, idp, username, domain, "").await
}

/// [`sign_in`] started with `query` on the login URL (e.g. `?return_to=…`).
async fn sign_in_from(
    app: &Router,
    idp: &Idp,
    username: &str,
    domain: &str,
    query: &str,
) -> (String, Option<String>) {
    let email = format!("{username}@{domain}");
    sign_in_with(
        app,
        idp,
        "mock",
        &format!("sub-{username}"),
        &email,
        false,
        query,
    )
    .await
}

/// The browser side of a login through provider `provider` as `subject`
/// with `email` (verified unless `unverified`).
async fn sign_in_with(
    app: &Router,
    idp: &Idp,
    provider: &str,
    subject: &str,
    email: &str,
    unverified: bool,
    query: &str,
) -> (String, Option<String>) {
    let login = format!("/api/v1/auth/sso/{provider}/login{query}");
    let (status, headers, _) = send(app, "GET", &login, Some(""), None).await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    let authorize = Url::parse(&location(&headers)).unwrap();
    let param = |name: &str| {
        authorize
            .query_pairs()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.into_owned())
    };
    let github = authorize
        .as_str()
        .starts_with(&format!("{}/login/oauth/authorize", idp.issuer));
    assert!(
        github
            || authorize
                .as_str()
                .starts_with(&format!("{}/authorize", idp.issuer)),
        "{authorize}"
    );
    assert_eq!(
        param("redirect_uri").unwrap(),
        format!("http://localhost/api/v1/auth/sso/{provider}/callback")
    );
    assert_eq!(param("code_challenge_method").unwrap(), "S256");
    // GitHub is plain OAuth2: no nonce, no `openid` scope.
    assert_eq!(param("nonce").is_none(), github);
    let state = param("state").unwrap();
    *idp.login.lock().unwrap() = Login {
        nonce: param("nonce").unwrap_or_default(),
        challenge: param("code_challenge").unwrap(),
        subject: subject.into(),
        username: email.split('@').next().unwrap().into(),
        email: email.into(),
        unverified,
    };
    let state_cookie = set_cookie(&headers, "sun_sso_state");
    let callback = format!("/api/v1/auth/sso/{provider}/callback?code=good-code&state={state}");
    let (status, headers, _) = send(app, "GET", &callback, Some(&state_cookie), None).await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    let session = headers
        .get_all(header::SET_COOKIE)
        .iter()
        .any(|v| v.to_str().unwrap().starts_with("sun_token="))
        .then(|| set_cookie(&headers, "sun_token"));
    // The state is single use: replaying the callback fails.
    let (_, replay, _) = send(app, "GET", &callback, Some(&state_cookie), None).await;
    assert_eq!(location(&replay), "/#/login/failed");
    (location(&headers), session)
}

const APPROVAL: &str = "
name: gatekept
steps:
  - id: approve
    type: approval
    who: [role:member]
";

#[tokio::test]
async fn auth_oidc_login_roles_and_audit() {
    let idp = mock_idp().await;
    let k = oidc_kernel(&idp, &[]).await;
    let app = &k.app;

    let (status, _, providers) = send(app, "GET", "/api/v1/auth", Some(""), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        providers["providers"],
        json!([{ "id": "mock", "name": "Mock IdP", "kind": "oidc" }])
    );
    // Token sign-in is gone: the token is the CLI's bearer only.
    let (status, _, _) = send(
        app,
        "POST",
        "/api/v1/session",
        Some(""),
        Some(json!({ "token": TOKEN })),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    // A callback without the browser's state cookie, or with a bad code, fails.
    let (_, headers, _) = send(app, "GET", "/api/v1/auth/sso/mock/login", Some(""), None).await;
    let state = Url::parse(&location(&headers))
        .unwrap()
        .query_pairs()
        .find(|(k, _)| k == "state")
        .unwrap()
        .1
        .into_owned();
    let forged = format!("/api/v1/auth/sso/mock/callback?code=good-code&state={state}");
    let (_, headers, _) = send(app, "GET", &forged, Some("sun_sso_state=other"), None).await;
    assert_eq!(location(&headers), "/#/login/failed");

    // Alice, invited as a viewer, signs in through the provider (a
    // deployment owner with an email exists, so she isn't the first).
    person(app, "root", Role::Viewer, true).await;
    invite(&k, "alice", "corp.io", Role::Viewer).await;
    let (landing, session) = sign_in(app, &idp, "alice", "corp.io").await;
    assert_eq!(landing, "/");
    let alice = session.unwrap();
    let (status, _, me) = send(app, "GET", "/api/v1/me", Some(&alice), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        (
            me["user"].as_str(),
            me["memberships"][0]["role"].as_str(),
            me["via"].as_str()
        ),
        (Some("alice"), Some("viewer"), Some("sso"))
    );
    assert_eq!(
        (&me["is_owner"], &me["email_verified"]),
        (&json!(false), &json!(true))
    );

    // Viewer: reads yes, writes and admin routes no.
    let (status, _, _) = send(app, "GET", "/api/v1/tasks", Some(&alice), None).await;
    assert_eq!(status, StatusCode::OK);
    let workflow = json!({ "source_yaml": APPROVAL });
    let (status, _, error) = send(
        app,
        "POST",
        "/api/v1/workflows",
        Some(&alice),
        Some(workflow.clone()),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(error["error"]["code"], "forbidden");
    for path in ["/api/v1/members", "/api/v1/audit"] {
        let (status, _, _) = send(app, "GET", path, Some(&alice), None).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{path}");
    }

    // The token holder (an owner) lists members and makes alice an admin
    // of Main.
    let (_, _, members) = send(app, "GET", "/api/v1/members", None, None).await;
    let listed = members
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["username"] == "alice")
        .unwrap();
    assert_eq!(listed["email"], "alice@corp.io");
    let alice_id: sun_core::MemberId = listed["id"].as_str().unwrap().parse().unwrap();
    let role = format!(
        "/api/v1/workspaces/{}/members/{alice_id}",
        WorkspaceId::MAIN
    );
    let (status, _, changed) =
        send(app, "PUT", &role, None, Some(json!({ "role": "admin" }))).await;
    assert_eq!(status, StatusCode::OK, "{changed}");

    // The new role applies at once: alice (an admin) approves `who: [role:member]`.
    let (status, _, workflow) = send(
        app,
        "POST",
        "/api/v1/workflows",
        Some(&alice),
        Some(workflow),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{workflow}");
    let task = json!({ "title": "Ship it", "workflow_id": workflow["id"] });
    let (status, _, task) = send(app, "POST", "/api/v1/tasks", Some(&alice), Some(task)).await;
    assert_eq!(status, StatusCode::CREATED, "{task}");
    let task_path = format!("/api/v1/tasks/{}", task["id"].as_str().unwrap());
    for _ in 0..200 {
        let (_, _, view) = send(app, "GET", &task_path, Some(&alice), None).await;
        if view["status"] == "waiting_approval" {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let approve = format!("{task_path}/approve");
    let (status, _, view) = send(app, "POST", &approve, Some(&alice), Some(json!({}))).await;
    assert_eq!(status, StatusCode::OK, "{view}");

    // Every security-relevant action is in the audit log, newest first.
    let (status, _, audit) = send(app, "GET", "/api/v1/audit", None, None).await;
    assert_eq!(status, StatusCode::OK);
    let rows: Vec<(String, String, String, String)> = audit
        .as_array()
        .unwrap()
        .iter()
        .map(|e| {
            let field = |k: &str| e[k].as_str().unwrap().to_owned();
            (
                field("actor"),
                field("action"),
                field("target"),
                field("detail"),
            )
        })
        .collect();
    let has = |actor: &str, action: &str, detail: &str| {
        rows.iter()
            .any(|r| r.0 == actor && r.1 == action && r.3 == detail)
    };
    assert!(
        has("user:alice", "login", "signed in with sso:mock"),
        "{rows:?}"
    );
    assert!(has("user:alice", "approval", "approved"), "{rows:?}");
    assert!(
        has(
            "user:me",
            "settings_change",
            "sso mock oidc saved, secret set"
        ),
        "{rows:?}"
    );
    assert!(has("user:me", "role_change", "viewer -> admin"), "{rows:?}");
    assert_eq!(rows.len(), 5, "{rows:?}");

    // Signing out ends the session.
    let (status, headers, _) = send(app, "DELETE", "/api/v1/session", Some(&alice), None).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert!(set_cookie(&headers, "sun_token").ends_with('='));
    let (status, _, _) = send(app, "GET", "/api/v1/me", Some(&alice), None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    // Deactivated: her session ends and the provider can't let her
    // back in until she is reactivated.
    let (_, session) = sign_in(app, &idp, "alice", "corp.io").await;
    let alice = session.unwrap();
    let id = alice_id;
    let (status, _, member) = send(
        app,
        "POST",
        &format!("/api/v1/members/{id}/deactivate"),
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{member}");
    assert!(member["deactivated_at"].is_string(), "{member}");
    let (status, _, _) = send(app, "GET", "/api/v1/me", Some(&alice), None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let (landing, session) = sign_in(app, &idp, "alice", "corp.io").await;
    assert_eq!((landing.as_str(), session), ("/#/login/denied", None));
    let activate = format!("/api/v1/members/{id}/activate");
    let (status, _, _) = send(app, "POST", &activate, None, None).await;
    assert_eq!(status, StatusCode::OK);
    let (landing, session) = sign_in(app, &idp, "alice", "corp.io").await;
    assert_eq!(landing, "/");
    assert!(session.is_some());
}

#[tokio::test]
async fn auth_oidc_allowed_domains_and_no_provider() {
    let idp = mock_idp().await;
    let k = oidc_kernel(&idp, &["corp.io"]).await;
    let (landing, session) = sign_in(&k.app, &idp, "mallory", "gmail.com").await;
    assert_eq!((landing.as_str(), session), ("/#/login/denied", None));
    // Sign-up closed: an allowed domain alone doesn't let anyone in, a
    // pending invitation does.
    let closed = invite_only(&k);
    let (landing, session) = sign_in(&closed, &idp, "carol", "corp.io").await;
    assert_eq!((landing.as_str(), session), ("/#/login/invite", None));
    let invitation = json!({ "email": "dora@corp.io", "role": "member" });
    let path = format!("/api/v1/workspaces/{}/invitations", WorkspaceId::MAIN);
    let (status, _, _) = send(&k.app, "POST", &path, None, Some(invitation)).await;
    assert_eq!(status, StatusCode::CREATED);
    let (landing, session) = sign_in(&closed, &idp, "dora", "corp.io").await;
    assert_eq!(landing, "/", "invited");
    let (_, _, mine) = send(
        &closed,
        "GET",
        "/api/v1/me/invitations",
        session.as_deref(),
        None,
    )
    .await;
    assert_eq!(mine[0]["role"], "member", "{mine}");
    // Sign-up open: the provider's verified email makes the account, in no
    // workspace yet; the first verified person owns the deployment.
    let (landing, session) = sign_in(&k.app, &idp, "carol", "corp.io").await;
    assert_eq!(landing, "/");
    let (_, _, me) = send(&k.app, "GET", "/api/v1/me", session.as_deref(), None).await;
    assert_eq!(
        (
            &me["email"],
            &me["email_verified"],
            &me["is_owner"],
            &me["memberships"]
        ),
        (
            &json!("carol@corp.io"),
            &json!(true),
            &json!(false),
            &json!([])
        ),
        "dora verified first: {me}"
    );
    let (_, _, members) = send(&k.app, "GET", "/api/v1/members", None, None).await;
    let dora = members
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["email"] == "dora@corp.io");
    assert_eq!(dora.unwrap()["is_owner"], true, "{members}");
    invite(&k, "bob", "corp.io", Role::Viewer).await;
    let (landing, session) = sign_in(&k.app, &idp, "bob", "CORP.io").await;
    assert_eq!(landing, "/");
    let bob = session.unwrap();

    // The session is stored: it survives a kernel restart on the same store.
    let app = restart(&k, None).await;
    let (status, _, me) = send(&app, "GET", "/api/v1/me", Some(&bob), None).await;
    assert_eq!((status, me["user"].as_str()), (StatusCode::OK, Some("bob")));

    // No provider set up: none is offered and the routes are 404.
    let k = kernel_with(None).await;
    let (_, _, providers) = send(&k.app, "GET", "/api/v1/auth", Some(""), None).await;
    assert_eq!(
        providers,
        json!({ "providers": [], "local": true, "allow_signup": true,
            "workspace_creation_disabled": false })
    );
    let (status, _, _) = send(&k.app, "GET", "/api/v1/auth/sso/mock/login", Some(""), None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

/// The role view is stored per member, so a second browser
/// (a new session) sees it; it is per user and only template roles go.
#[tokio::test]
async fn view_role_follows_the_member() {
    let idp = mock_idp().await;
    let k = oidc_kernel(&idp, &[]).await;
    invite(&k, "bob", "corp.io", Role::Member).await;
    let (_, bob) = sign_in(&k.app, &idp, "bob", "corp.io").await;
    let bob = bob.unwrap();
    let (_, _, me) = send(&k.app, "GET", "/api/v1/me", Some(&bob), None).await;
    assert_eq!(me["view_role"], Value::Null);

    let put = |role: Value| Some(json!({ "view_role": role }));
    let path = "/api/v1/me/view_role";
    let (status, _, body) = send(&k.app, "PUT", path, Some(&bob), put(json!("marketing"))).await;
    assert_eq!(
        (status, &body["view_role"]),
        (StatusCode::OK, &json!("marketing"))
    );
    let (status, _, _) = send(&k.app, "PUT", path, Some(&bob), put(json!("hacker"))).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);

    let (_, other_browser) = sign_in(&k.app, &idp, "bob", "corp.io").await;
    let other_browser = other_browser.unwrap();
    assert_ne!(other_browser, bob);
    let (_, _, me) = send(&k.app, "GET", "/api/v1/me", Some(&other_browser), None).await;
    assert_eq!(me["view_role"], "marketing");
    let (_, _, me) = send(&k.app, "GET", "/api/v1/me", None, None).await;
    assert_eq!(me["view_role"], Value::Null, "per user, not per ecosystem");

    let (status, _, _) = send(&k.app, "PUT", path, Some(&bob), put(Value::Null)).await;
    assert_eq!(status, StatusCode::OK);
    let (_, _, me) = send(&k.app, "GET", "/api/v1/me", Some(&bob), None).await;
    assert_eq!(me["view_role"], Value::Null);
}

#[tokio::test]
async fn auth_oidc_keeps_the_deep_link_and_plan_seats() {
    let idp = mock_idp().await;
    let k = oidc_kernel(&idp, &[]).await;
    invite(&k, "alice", "corp.io", Role::Viewer).await;
    // A notification's deep link survives the provider round trip.
    let deep = "?return_to=%23%2Fapprove%2F01JAR%2Freview";
    let (landing, _) = sign_in_from(&k.app, &idp, "alice", "corp.io", deep).await;
    assert_eq!(landing, "/#/approve/01JAR/review");
    // Anything but a UI hash lands on `/`: no open redirect.
    for evil in ["?return_to=%2F%2Fevil.test", "?return_to=%23%2F%3Cx%3E"] {
        let (landing, _) = sign_in_from(&k.app, &idp, "alice", "corp.io", evil).await;
        assert_eq!(landing, "/", "{evil}");
    }

    // The plan has one seat: alice holds it, so no one else can be invited;
    // alice still gets in.
    common::plan(
        &k,
        WorkspaceId::MAIN,
        sun_core::entitlement::Limits {
            seats: Some(1),
            hosts: Some(1),
            ..Default::default()
        },
    );
    let bob = json!({ "email": "bob@corp.io", "username": "bob" });
    let (status, _, body) = send(&k.app, "POST", "/api/v1/members", None, Some(bob)).await;
    assert_eq!(
        (status, &body["error"]["code"]),
        (StatusCode::FORBIDDEN, &json!("plan_limit"))
    );
    let (landing, session) = sign_in(&k.app, &idp, "alice", "corp.io").await;
    assert_eq!(landing, "/");
    assert!(session.is_some());

    // One host (the test kernel's) fills `hosts: 1`: no new enrolment token.
    let (status, _, body) = send(
        &k.app,
        "POST",
        "/api/v1/workspaces/00000000000000000000000001/hosts/tokens",
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(body["error"]["code"], "plan_limit");
    assert_eq!(
        body["error"]["message"],
        "Plan limit reached (hosts: 1 on the business plan). Remove one or upgrade the plan."
    );
}

/// The member a provider linked: `(issuer, subject)`.
async fn identity_of(k: &common::Kernel, email: &str) -> (String, String) {
    let mut tx = k.store.begin().await.unwrap();
    let member = tx.member_by_email(email).await.unwrap().unwrap();
    tx.commit().await.unwrap();
    (member.issuer, member.subject)
}

/// Providers managed in Settings → Sign-in (secrets sealed, never
/// shown), the Google and GitHub presets through the mock, invite-only
/// linking, allowed domains, deactivation and removal.
#[tokio::test]
async fn auth_sso_providers_google_github_and_settings() {
    let idp = mock_idp().await;
    let k = kernel_with(Some(idp.presets())).await;
    let closed = invite_only(&k);
    let app = &closed;
    let put = |id: &str, body: Value| {
        let path = format!("/api/v1/settings/sso/{id}");
        async move { send(app, "PUT", &path, None, Some(body)).await }
    };

    // A new provider needs its secret; ids and fields are checked.
    let google = json!({ "kind": "google", "name": "Google", "client_id": CLIENT_ID });
    let (status, _, body) = put("google", google.clone()).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    let mut with_secret = google.clone();
    with_secret["client_secret"] = json!("client-secret");
    let (status, _, _) = put("Bad-Id", with_secret.clone()).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    let oidc =
        json!({ "kind": "oidc", "name": "Keycloak", "client_id": CLIENT_ID, "client_secret": "s" });
    let (status, _, _) = put("kc", oidc).await;
    assert_eq!(
        status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "oidc needs issuer_url"
    );

    let (status, _, view) = put("google", with_secret).await;
    assert_eq!(status, StatusCode::OK, "{view}");
    let github = json!({ "kind": "github", "name": "GitHub", "client_id": CLIENT_ID, "client_secret": "client-secret" });
    let (status, _, view) = put("github", github).await;
    assert_eq!(status, StatusCode::OK, "{view}");
    assert_eq!(
        view["redirect_url"],
        "http://localhost/api/v1/auth/sso/{id}/callback"
    );
    assert_eq!(view["providers"][0]["id"], "google");
    assert_eq!(view["providers"][0]["has_secret"], true);
    assert_eq!(view["providers"][1]["issuer_url"], Value::Null);
    assert!(!view.to_string().contains("client-secret"), "{view}");
    // Saving again without a secret keeps the stored one.
    let rename = json!({ "kind": "google", "name": "Google Workspace", "client_id": CLIENT_ID });
    let (status, _, view) = put("google", rename).await;
    assert_eq!(status, StatusCode::OK, "{view}");
    assert_eq!(view["providers"][0]["has_secret"], true);

    let (_, _, offered) = send(app, "GET", "/api/v1/auth", Some(""), None).await;
    assert_eq!(
        offered["providers"],
        json!([
            { "id": "google", "name": "Google Workspace", "kind": "google" },
            { "id": "github", "name": "GitHub", "kind": "github" },
        ])
    );

    // Google: an invited member links by their verified email.
    person(app, "root", Role::Viewer, true).await;
    invite(&k, "alice", "corp.io", Role::Viewer).await;
    let (landing, session) =
        sign_in_with(app, &idp, "google", "g-1", "alice@corp.io", false, "").await;
    assert_eq!(landing, "/");
    let alice = session.unwrap();
    assert_eq!(
        identity_of(&k, "alice@corp.io").await,
        (idp.issuer.clone(), "g-1".into())
    );
    // Only deployment owners see or change providers.
    let (status, _, _) = send(app, "GET", "/api/v1/settings/sso", Some(&alice), None).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, _, _) = send(
        app,
        "DELETE",
        "/api/v1/settings/sso/github",
        Some(&alice),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    // GitHub: the subject is the numeric user id, the email the primary
    // verified one; nobody uninvited, and no unverified email, gets in.
    invite(&k, "bob", "corp.io", Role::Viewer).await;
    let (landing, session) =
        sign_in_with(app, &idp, "github", "4242", "carol@corp.io", false, "").await;
    assert_eq!((landing.as_str(), session), ("/#/login/invite", None));
    let (landing, session) =
        sign_in_with(app, &idp, "github", "4243", "bob@corp.io", true, "").await;
    assert_eq!((landing.as_str(), session), ("/#/login/invite", None));
    let (landing, session) =
        sign_in_with(app, &idp, "github", "4243", "bob@corp.io", false, "").await;
    assert_eq!(landing, "/");
    let bob = session.unwrap();
    assert_eq!(
        identity_of(&k, "bob@corp.io").await,
        ("github".into(), "4243".into())
    );
    let (_, _, me) = send(app, "GET", "/api/v1/me", Some(&bob), None).await;
    assert_eq!(
        (me["user"].as_str(), me["via"].as_str()),
        (Some("bob"), Some("sso"))
    );

    // A state from one provider doesn't finish a login at another.
    let (_, headers, _) = send(app, "GET", "/api/v1/auth/sso/google/login", Some(""), None).await;
    let state = Url::parse(&location(&headers))
        .unwrap()
        .query_pairs()
        .find(|(k, _)| k == "state")
        .unwrap()
        .1
        .into_owned();
    let cookie = set_cookie(&headers, "sun_sso_state");
    let cross = format!("/api/v1/auth/sso/github/callback?code=good-code&state={state}");
    let (_, headers, _) = send(app, "GET", &cross, Some(&cookie), None).await;
    assert_eq!(location(&headers), "/#/login/failed");

    // Deactivated: GitHub can't let bob back in.
    let (_, _, members) = send(app, "GET", "/api/v1/members", None, None).await;
    let bob_id = members
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["username"] == "bob")
        .unwrap()["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let deactivate = format!("/api/v1/members/{bob_id}/deactivate");
    let (status, _, _) = send(app, "POST", &deactivate, None, None).await;
    assert_eq!(status, StatusCode::OK);
    let (landing, session) =
        sign_in_with(app, &idp, "github", "4243", "bob@corp.io", false, "").await;
    assert_eq!((landing.as_str(), session), ("/#/login/denied", None));

    // Allowed domains: alice's verified corp.io email passes, gmail doesn't.
    let github = json!({ "kind": "github", "name": "GitHub", "client_id": CLIENT_ID, "allowed_domains": ["Corp.io"] });
    let (status, _, view) = put("github", github).await;
    assert_eq!(status, StatusCode::OK, "{view}");
    assert_eq!(view["providers"][1]["allowed_domains"], json!(["corp.io"]));
    let (landing, _) =
        sign_in_with(app, &idp, "github", "4244", "mallory@gmail.com", false, "").await;
    assert_eq!(landing, "/#/login/denied");
    let (landing, _) = sign_in_with(app, &idp, "github", "4245", "alice@corp.io", false, "").await;
    assert_eq!(
        landing, "/#/login/invite",
        "alice is linked to Google already"
    );

    // Removing a provider drops it, its secret and its routes.
    let (status, _, _) = send(app, "DELETE", "/api/v1/settings/sso/github", None, None).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (status, _, _) = send(app, "DELETE", "/api/v1/settings/sso/github", None, None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _, _) = send(app, "GET", "/api/v1/auth/sso/github/login", Some(""), None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (_, _, secrets) = send(app, "GET", "/api/v1/secrets", None, None).await;
    let names = secrets.to_string();
    assert!(
        names.contains("SSO_GOOGLE_SECRET") && !names.contains("SSO_GITHUB_SECRET"),
        "{names}"
    );

    // A provider whose secret is gone could only fail: the login page hides
    // it, Settings still lists it without a secret.
    let secret = "/api/v1/secrets/ecosystem/00000000000000000000000000/SSO_GOOGLE_SECRET";
    let (status, _, _) = send(app, "DELETE", secret, None, None).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (_, _, offered) = send(app, "GET", "/api/v1/auth", Some(""), None).await;
    assert_eq!(offered["providers"], json!([]));
    let login = "/api/v1/auth/sso/google/login";
    let (status, _, _) = send(app, "GET", login, Some(""), None).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "a provider without a secret");
    let (_, _, view) = send(app, "GET", "/api/v1/settings/sso", None, None).await;
    assert_eq!(
        (
            &view["providers"][0]["id"],
            &view["providers"][0]["has_secret"]
        ),
        (&json!("google"), &json!(false))
    );

    // Every change is audited, never the secret.
    let (_, _, audit) = send(app, "GET", "/api/v1/audit", None, None).await;
    let details: Vec<&str> = audit
        .as_array()
        .unwrap()
        .iter()
        .filter(|e| e["action"] == "settings_change")
        .map(|e| e["detail"].as_str().unwrap())
        .collect();
    assert_eq!(
        details,
        [
            "sso github github removed",
            "sso github github saved",
            "sso google google saved",
            "sso github github saved, secret set",
            "sso google google saved, secret set",
        ]
    );
    // Logins name their provider.
    let logins: Vec<(&str, &str)> = audit
        .as_array()
        .unwrap()
        .iter()
        .filter(|e| e["action"] == "login")
        .map(|e| (e["target"].as_str().unwrap(), e["detail"].as_str().unwrap()))
        .collect();
    assert_eq!(
        logins,
        [
            ("bob", "signed in with sso:github"),
            ("alice", "signed in with sso:google")
        ]
    );
}

fn sha256_hex(token: &str) -> String {
    Sha256::digest(token.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// `POST /session` as `email` with `password`: the session cookie, or the
/// status when it failed.
async fn password_session(app: &Router, email: &str, password: &str) -> Result<String, StatusCode> {
    let body = json!({ "email": email, "password": password });
    let (status, headers, _) = send(app, "POST", "/api/v1/session", Some(""), Some(body)).await;
    match status {
        StatusCode::NO_CONTENT => Ok(set_cookie(&headers, "sun_token")),
        status => Err(status),
    }
}

/// `sun serve` seeds the owner as admin@sun.local with the kernel
/// token as the password; they must replace the password, then the
/// email. Token and old `{email, otp}` bodies are 401. Sessions survive a
/// restart.
#[tokio::test]
async fn auth_first_run_owner_signs_in_with_the_token() {
    let k = kernel().await;
    let app = &k.app;
    assert_eq!(
        k.api.seed_owner().await.unwrap().as_deref(),
        Some("admin@sun.local")
    );
    assert_eq!(k.api.seed_owner().await.unwrap(), None, "seeded once");

    for body in [
        json!({ "token": TOKEN }),
        json!({ "email": "admin@sun.local", "otp": "123456" }),
        json!({ "email": "admin@sun.local", "password": "not the token" }),
    ] {
        let (status, _, _) = send(app, "POST", "/api/v1/session", Some(""), Some(body)).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
    }
    let session = password_session(app, "admin@sun.local", TOKEN)
        .await
        .unwrap();
    assert!(!session.contains(TOKEN), "{session}");
    let me_cookie = Some(session.as_str());
    let (status, _, me) = send(app, "GET", "/api/v1/me", me_cookie, None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        (
            me["user"].as_str(),
            &me["is_owner"],
            &me["needs_email"],
            me["via"].as_str()
        ),
        (Some("me"), &json!(true), &json!(true), Some("local"))
    );
    // A deployment owner: every workspace and every `who: role:` without
    // a membership.
    assert_eq!(me["memberships"], json!([]));
    let (status, _, error) = send(app, "GET", "/api/v1/tasks", me_cookie, None).await;
    assert_eq!(
        (status, &error["error"]["code"]),
        (StatusCode::FORBIDDEN, &json!("password_required"))
    );
    // Without a rotation hook (`sun serve` has one) the token stays.
    let own = Some(json!({ "password": "the boss's own password" }));
    let (status, _, _) = send(app, "PUT", "/api/v1/me/password", me_cookie, own).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (status, _, error) = send(app, "GET", "/api/v1/tasks", me_cookie, None).await;
    assert_eq!(
        (status, &error["error"]["code"]),
        (StatusCode::FORBIDDEN, &json!("email_required"))
    );
    let email = Some(json!({ "email": "boss@corp.io" }));
    let (status, _, _) = send(app, "PUT", "/api/v1/me/email", me_cookie, email).await;
    assert_eq!(status, StatusCode::OK);
    let (_, _, me) = send(app, "GET", "/api/v1/me", me_cookie, None).await;
    assert_eq!(me["needs_email"], false);
    let (status, _, _) = send(app, "GET", "/api/v1/tasks", me_cookie, None).await;
    assert_eq!(status, StatusCode::OK);
    password_session(app, "boss@corp.io", "the boss's own password")
        .await
        .unwrap();
    // The bearer token is still the CLI's credential.
    let (status, _, _) = send(app, "GET", "/api/v1/tasks", None, None).await;
    assert_eq!(status, StatusCode::OK);

    // An expired session is refused; a live one survives a restart, and
    // signing out deletes it.
    let past = chrono::Utc::now() - chrono::Duration::seconds(1);
    let mut tx = k.store.begin().await.unwrap();
    let owner = tx.members().await.unwrap()[0].id;
    tx.insert_login_session(&sha256_hex("old"), owner, past)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    let (status, _, _) = send(app, "GET", "/api/v1/me", Some("sun_token=old"), None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let app = &restart(&k, None).await;
    let (status, _, _) = send(app, "GET", "/api/v1/me", me_cookie, None).await;
    assert_eq!(status, StatusCode::OK);
    let (status, _, _) = send(app, "DELETE", "/api/v1/session", me_cookie, None).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (status, _, _) = send(app, "GET", "/api/v1/me", me_cookie, None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    // An older owner row (onboarding gave it an email, no
    // password) gets the token as its password, keeping its email.
    let k = kernel().await;
    let mut tx = k.store.begin().await.unwrap();
    tx.upsert_member("token", "me", "me", Some("old@corp.io"), true)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    assert_eq!(
        k.api.seed_owner().await.unwrap().as_deref(),
        Some("old@corp.io")
    );
    password_session(&k.app, "old@corp.io", TOKEN)
        .await
        .unwrap();

    // An owner whose password is still the (printed) token
    // must change it at the next start; one with their own password keeps it.
    let mut tx = k.store.begin().await.unwrap();
    let owner = tx.members().await.unwrap()[0].id;
    let hash = tx.password_hash(owner).await.unwrap();
    tx.set_password(owner, hash.as_deref(), false)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    assert_eq!(k.api.seed_owner().await.unwrap(), None);
    let mut tx = k.store.begin().await.unwrap();
    assert!(tx.get_member(owner).await.unwrap().must_set_password);
    tx.set_password(owner, Some("$argon2id$not-the-token"), false)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    assert_eq!(k.api.seed_owner().await.unwrap(), None);
    let mut tx = k.store.begin().await.unwrap();
    assert!(!tx.get_member(owner).await.unwrap().must_set_password);
}

/// Admins create accounts with a password (no SMTP needed), set
/// passwords (ending sessions, forcing a change by default), and deactivate
/// and reactivate members; deactivated members can't sign in, and the last
/// active owner stays.
#[tokio::test]
async fn auth_admin_set_passwords_and_deactivation() {
    let k = kernel().await;
    let app = &k.app;
    let bob = |password: &str| {
        Some(json!({ "email": "bob@corp.io", "username": "bob",
            "role": "member", "password": password }))
    };
    let (status, _, _) = send(app, "POST", "/api/v1/members", None, bob("short")).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    // No SMTP, yet the account is made, and no mail goes out.
    let first = bob("a long first password");
    let (status, _, member) = send(app, "POST", "/api/v1/members", None, first).await;
    assert_eq!(status, StatusCode::CREATED, "{member}");
    assert_eq!(
        (&member["has_password"], &member["must_set_password"]),
        (&json!(true), &json!(true))
    );
    assert!(!member.to_string().contains("argon2"));
    assert!(k.mails.sent.lock().unwrap().is_empty());
    let id = member["id"].as_str().unwrap().to_owned();

    // The admin-set password forces a change at the first sign-in.
    let bob_cookie = password_session(app, "bob@corp.io", "a long first password")
        .await
        .unwrap();
    let bob_session = Some(bob_cookie.as_str());
    let (status, _, error) = send(app, "GET", "/api/v1/tasks", bob_session, None).await;
    assert_eq!(
        (status, &error["error"]["code"]),
        (StatusCode::FORBIDDEN, &json!("password_required"))
    );
    let mine = Some(json!({ "password": "bob's own password" }));
    let (status, _, _) = send(app, "PUT", "/api/v1/me/password", bob_session, mine).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (status, _, _) = send(app, "GET", "/api/v1/tasks", bob_session, None).await;
    assert_eq!(status, StatusCode::OK);

    // An admin sets it again without forcing a change: his sessions end.
    let set = format!("/api/v1/members/{id}/password");
    let body = Some(json!({ "password": "admin chose this one", "must_change": false }));
    let (status, _, member) = send(app, "PUT", &set, None, body).await;
    assert_eq!(status, StatusCode::OK, "{member}");
    assert_eq!(member["must_set_password"], false);
    assert!(!member.to_string().contains("argon2"));
    let (status, _, _) = send(app, "GET", "/api/v1/me", bob_session, None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(
        password_session(app, "bob@corp.io", "bob's own password").await,
        Err(StatusCode::UNAUTHORIZED)
    );
    let bob_cookie = password_session(app, "bob@corp.io", "admin chose this one")
        .await
        .unwrap();
    let bob_session = Some(bob_cookie.as_str());
    let (status, _, _) = send(app, "GET", "/api/v1/tasks", bob_session, None).await;
    assert_eq!(status, StatusCode::OK);
    // A member can't set someone's password.
    let body = Some(json!({ "password": "bob sets his own" }));
    let (status, _, _) = send(app, "PUT", &set, bob_session, body).await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    // Deactivated: sessions end, and the password no longer works.
    let deactivate = format!("/api/v1/members/{id}/deactivate");
    let (status, _, member) = send(app, "POST", &deactivate, None, None).await;
    assert_eq!(status, StatusCode::OK);
    assert!(member["deactivated_at"].is_string());
    let (status, _, _) = send(app, "GET", "/api/v1/me", bob_session, None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(
        password_session(app, "bob@corp.io", "admin chose this one").await,
        Err(StatusCode::UNAUTHORIZED)
    );
    let activate = format!("/api/v1/members/{id}/activate");
    let (status, _, member) = send(app, "POST", &activate, None, None).await;
    assert_eq!(
        (status, &member["deactivated_at"]),
        (StatusCode::OK, &Value::Null)
    );
    password_session(app, "bob@corp.io", "admin chose this one")
        .await
        .unwrap();

    // The last active owner can't be deactivated; with a second one, one can.
    let owner = |name: &str| {
        Some(
            json!({ "email": format!("{name}@corp.io"), "username": name,
            "is_owner": true, "password": "an owner's password" }),
        )
    };
    let (_, _, olive) = send(app, "POST", "/api/v1/members", None, owner("olive")).await;
    let olive = format!(
        "/api/v1/members/{}/deactivate",
        olive["id"].as_str().unwrap()
    );
    let (status, _, error) = send(app, "POST", &olive, None, None).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{error}");
    send(app, "POST", "/api/v1/members", None, owner("otto")).await;
    let (status, _, _) = send(app, "POST", &olive, None, None).await;
    assert_eq!(status, StatusCode::OK);

    let (_, _, audit) = send(app, "GET", "/api/v1/audit?limit=500", None, None).await;
    let audit = audit.to_string();
    for expected in [
        "created with a password bob@corp.io into Main as member",
        "password set by an admin",
        "\"deactivated\"",
        "\"reactivated\"",
    ] {
        assert!(audit.contains(expected), "{expected}: {audit}");
    }
    assert!(!audit.contains("admin chose this one"));
}

#[tokio::test]
async fn acl_permissions_are_per_capability() {
    use sun_store::MembershipRepo;
    let k = kernel().await;
    let app = &k.app;
    let later = chrono::Utc::now() + chrono::Duration::hours(1);
    let mut tx = k.store.begin().await.unwrap();
    let mut ids = Vec::new();
    for (name, role) in [("ada", Role::Admin), ("max", Role::Member)] {
        let member = tx
            .upsert_member(
                "https://idp",
                name,
                name,
                Some(&format!("{name}@x.io")),
                false,
            )
            .await
            .unwrap();
        tx.add_member(WorkspaceId::MAIN, member.id, role)
            .await
            .unwrap();
        tx.insert_login_session(&sha256_hex(name), member.id, later)
            .await
            .unwrap();
        ids.push(member.id);
    }
    tx.commit().await.unwrap();
    let (ada, max) = (Some("sun_token=ada"), Some("sun_token=max"));

    // The token holder is a deployment owner.
    let (_, _, me) = send(app, "GET", "/api/v1/me", None, None).await;
    assert_eq!(me["is_owner"], true);

    // Ecosystem secrets are the deployment's: not even an admin of Main.
    let (status, _, _) = send(app, "GET", "/api/v1/secrets", ada, None).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, _, _) = send(app, "GET", "/api/v1/secrets", max, None).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    // So is the ecosystem limit policy, reading it too.
    let on_limit = "/api/v1/settings/on_limit";
    let wait = json!({ "on_limit": "wait_until_reset" });
    let (status, _, _) = send(app, "PUT", on_limit, max, Some(wait.clone())).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, _, _) = send(app, "PUT", on_limit, ada, Some(wait.clone())).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, _, _) = send(app, "PUT", on_limit, None, Some(wait.clone())).await;
    assert_eq!(status, StatusCode::OK);
    let (status, _, _) = send(app, "GET", on_limit, max, None).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (_, _, read) = send(app, "GET", on_limit, None, None).await;
    assert_eq!(read, wait);

    // Only a deployment owner grants it.
    let owner = |id: &sun_core::MemberId| format!("/api/v1/members/{id}/owner");
    let yes = json!({ "is_owner": true });
    let (status, _, _) = send(app, "PUT", &owner(&ids[1]), ada, Some(yes.clone())).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, _, member) = send(app, "PUT", &owner(&ids[1]), None, Some(yes)).await;
    assert_eq!(
        (status, &member["is_owner"]),
        (StatusCode::OK, &json!(true))
    );
    let (status, _, _) = send(app, "GET", "/api/v1/secrets", max, None).await;
    assert_eq!(status, StatusCode::OK);
}

/// The 6-digit code in an invite mail.
fn code_in(mail: &sun_core::model::Mail) -> String {
    mail.body
        .split_whitespace()
        .find(|w| w.len() == 6 && w.bytes().all(|b| b.is_ascii_digit()))
        .unwrap()
        .to_owned()
}

/// Invite → code sign-in (typed in the password box) → forced
/// set-password → password sign-in; 409 without SMTP; the 5-try code
/// lockout; one error for every failure; the rate limit; re-invite,
/// deactivation; the owner's and a pre-invite member's email.
#[tokio::test]
async fn auth_invite_code_password_smtp_and_limits() {
    let k = kernel().await;
    let app = &k.app;
    let session = |email: &str, key: &str, value: &str| Some(json!({ "email": email, key: value }));
    let ann = json!({ "email": "Ann@Corp.io", "username": "ann",
        "role": "member" });

    // No SMTP yet: invites are refused, and nothing is stored.
    let (status, _, body) = send(app, "POST", "/api/v1/members", None, Some(ann.clone())).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap()
            .starts_with("Configure email first")
    );
    let (_, _, smtp) = send(app, "GET", "/api/v1/smtp", None, None).await;
    assert_eq!(smtp, json!({ "smtp": null, "has_password": false }));

    // SMTP: validated; the password is sealed and never comes back.
    let bad = json!({ "host": "smtp test", "port": 587, "from": "sun@corp.io", "tls": "starttls" });
    let (status, _, _) = send(app, "PUT", "/api/v1/smtp", None, Some(bad)).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    let smtp = json!({
        "host": "smtp.corp.io", "port": 587, "username": "sun", "from": "sun@corp.io",
        "tls": "starttls", "password": "smtp-pass-123",
    });
    let (status, _, saved) = send(app, "PUT", "/api/v1/smtp", None, Some(smtp)).await;
    assert_eq!(status, StatusCode::OK, "{saved}");
    assert_eq!(saved["has_password"], true);
    assert_eq!(saved["smtp"]["host"], "smtp.corp.io");
    assert!(!saved.to_string().contains("smtp-pass-123"));

    // The test mail goes to the caller: the owner gives their email first.
    let (status, _, _) = send(app, "POST", "/api/v1/smtp/test", None, None).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    let (_, _, me) = send(app, "GET", "/api/v1/me", None, None).await;
    assert_eq!(
        (me["needs_email"].as_bool(), &me["email"]),
        (Some(true), &Value::Null)
    );
    let owner = Some(json!({ "email": "Owner@Corp.io" }));
    let (status, _, _) = send(app, "PUT", "/api/v1/me/email", None, owner).await;
    assert_eq!(status, StatusCode::OK);
    let (_, _, me) = send(app, "GET", "/api/v1/me", None, None).await;
    assert_eq!(
        (me["needs_email"].as_bool(), me["email"].as_str()),
        (Some(false), Some("owner@corp.io"))
    );
    let (status, _, sent) = send(app, "POST", "/api/v1/smtp/test", None, None).await;
    assert_eq!(
        (status, &sent["sent_to"]),
        (StatusCode::OK, &json!("owner@corp.io"))
    );
    assert_eq!(
        k.mails.sent.lock().unwrap().last().unwrap().1.as_deref(),
        Some("smtp-pass-123"),
        "the mailer gets the sealed password"
    );

    // Invite ann: the mail carries the code and a link; a second invite of
    // the same email is refused.
    let (status, _, member) = send(app, "POST", "/api/v1/members", None, Some(ann.clone())).await;
    assert_eq!(status, StatusCode::CREATED, "{member}");
    assert_eq!(
        (member["email"].as_str(), member["has_password"].as_bool()),
        (Some("ann@corp.io"), Some(false))
    );
    let id = member["id"].as_str().unwrap().to_owned();
    let mail = k.mails.last();
    assert_eq!(mail.to, "ann@corp.io");
    assert!(mail.body.contains("https://sun.test/\n"), "{}", mail.body);
    let code = code_in(&mail);
    let (status, _, _) = send(app, "POST", "/api/v1/members", None, Some(ann)).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);

    // Every failure reads the same: wrong code, unknown email, no password yet.
    let (status, _, wrong) = send(
        app,
        "POST",
        "/api/v1/session",
        Some(""),
        session("ann@corp.io", "password", "000000"),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    for body in [
        session("nobody@corp.io", "password", "whatever-password"),
        session("ann@corp.io", "password", "whatever-password"),
        session("nobody@corp.io", "password", &code),
        session("ann@corp.io", "password", "12345"),
    ] {
        let (status, _, error) = send(app, "POST", "/api/v1/session", Some(""), body).await;
        assert_eq!((status, &error), (StatusCode::UNAUTHORIZED, &wrong));
    }

    // The code signs in once; then only setting a password is allowed.
    let (status, headers, _) = send(
        app,
        "POST",
        "/api/v1/session",
        Some(""),
        session("ANN@corp.io", "password", &code),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let ann_cookie = set_cookie(&headers, "sun_token");
    let ann_session = Some(ann_cookie.as_str());
    let (_, _, me) = send(app, "GET", "/api/v1/me", ann_session, None).await;
    assert_eq!(
        (me["must_set_password"].as_bool(), me["via"].as_str()),
        (Some(true), Some("local"))
    );
    let (status, _, error) = send(app, "GET", "/api/v1/tasks", ann_session, None).await;
    assert_eq!(
        (status, &error["error"]["code"]),
        (StatusCode::FORBIDDEN, &json!("password_required"))
    );
    let (status, _, _) = send(
        app,
        "POST",
        "/api/v1/session",
        Some(""),
        session("ann@corp.io", "password", &code),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "single use");
    let short = Some(json!({ "password": "too-short" }));
    let (status, _, _) = send(app, "PUT", "/api/v1/me/password", ann_session, short).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    let first = Some(json!({ "password": "first long password" }));
    let (status, _, _) = send(app, "PUT", "/api/v1/me/password", ann_session, first).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (status, _, _) = send(app, "GET", "/api/v1/tasks", ann_session, None).await;
    assert_eq!(status, StatusCode::OK);

    // Password sign-in; changing it later needs the current one.
    let good = session("ann@corp.io", "password", "first long password");
    let (status, _, _) = send(app, "POST", "/api/v1/session", Some(""), good).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let change = Some(json!({ "password": "second long password" }));
    let (status, _, _) = send(app, "PUT", "/api/v1/me/password", ann_session, change).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let change = Some(
        json!({ "password": "second long password", "current_password": "first long password" }),
    );
    let (status, _, _) = send(app, "PUT", "/api/v1/me/password", ann_session, change).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    // A member isn't an admin: no invites.
    let eve = json!({ "email": "eve@corp.io", "username": "eve" });
    let (status, _, _) = send(app, "POST", "/api/v1/members", ann_session, Some(eve)).await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    // Re-invite (forgotten password): the old password and session stop
    // working; the new code dies after five wrong tries.
    let reinvite = format!("/api/v1/members/{id}/invite");
    let (status, _, member) = send(app, "POST", &reinvite, None, None).await;
    assert_eq!(
        (status, member["has_password"].as_bool()),
        (StatusCode::OK, Some(false))
    );
    let (status, _, _) = send(app, "GET", "/api/v1/me", ann_session, None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let old = session("ann@corp.io", "password", "second long password");
    let (status, _, _) = send(app, "POST", "/api/v1/session", Some(""), old).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let code = code_in(&k.mails.last());
    let miss = if code == "000000" { "111111" } else { "000000" };
    for _ in 0..5 {
        let (status, _, _) = send(
            app,
            "POST",
            "/api/v1/session",
            Some(""),
            session("ann@corp.io", "password", miss),
        )
        .await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
    }
    let (status, _, _) = send(
        app,
        "POST",
        "/api/v1/session",
        Some(""),
        session("ann@corp.io", "password", &code),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "locked after five misses");

    // Rate limit: ten failures per email and IP, then 429 even for a right code.
    let (_, _, _) = send(app, "POST", &reinvite, None, None).await;
    let code = code_in(&k.mails.last());
    let mut limited = false;
    for _ in 0..12 {
        let wrong = session("ann@corp.io", "password", "not her password");
        let (status, _, _) = send(app, "POST", "/api/v1/session", Some(""), wrong).await;
        limited |= status == StatusCode::TOO_MANY_REQUESTS;
    }
    assert!(limited);
    let (status, _, _) = send(
        app,
        "POST",
        "/api/v1/session",
        Some(""),
        session("ann@corp.io", "password", &code),
    )
    .await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);

    // A member from before invites without an email adds it before anything else.
    let later = chrono::Utc::now() + chrono::Duration::hours(1);
    let mut tx = k.store.begin().await.unwrap();
    let old = tx
        .upsert_member("https://idp", "o1", "olga", None, false)
        .await
        .unwrap();
    tx.insert_login_session(&sha256_hex("olga"), old.id, later)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    let olga = Some("sun_token=olga");
    let (status, _, error) = send(app, "GET", "/api/v1/tasks", olga, None).await;
    assert_eq!(
        (status, &error["error"]["code"]),
        (StatusCode::FORBIDDEN, &json!("email_required"))
    );
    let taken = Some(json!({ "email": "ann@corp.io" }));
    let (status, _, _) = send(app, "PUT", "/api/v1/me/email", olga, taken).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    let mine = Some(json!({ "email": "olga@corp.io" }));
    let (status, _, _) = send(app, "PUT", "/api/v1/me/email", olga, mine.clone()).await;
    assert_eq!(status, StatusCode::OK);
    let (status, _, _) = send(app, "GET", "/api/v1/tasks", olga, None).await;
    assert_eq!(status, StatusCode::OK);
    let (status, _, _) = send(app, "PUT", "/api/v1/me/email", olga, mine).await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "set once; an admin changes it"
    );

    // Deactivation ends the member's sessions (`DELETE` is gone); every
    // admin action is audited.
    let deactivate = format!("/api/v1/members/{}/deactivate", old.id);
    let (status, _, _) = send(app, "POST", &deactivate, None, None).await;
    assert_eq!(status, StatusCode::OK);
    let (status, _, _) = send(app, "GET", "/api/v1/me", olga, None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let gone = format!("/api/v1/members/{}", old.id);
    let (status, _, _) = send(app, "DELETE", &gone, None, None).await;
    assert!(!status.is_success(), "{status}");
    let unknown = format!(
        "/api/v1/members/{}/deactivate",
        sun_core::MemberId::generate()
    );
    let (status, _, _) = send(app, "POST", &unknown, None, None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (_, _, audit) = send(app, "GET", "/api/v1/audit?limit=500", None, None).await;
    let details: Vec<String> = audit
        .as_array()
        .unwrap()
        .iter()
        .map(|e| {
            format!(
                "{} {} {}",
                e["action"].as_str().unwrap(),
                e["target"].as_str().unwrap(),
                e["detail"].as_str().unwrap()
            )
        })
        .collect();
    for expected in [
        "member_change ann invited ann@corp.io into Main as member",
        "member_change ann re-invited ann@corp.io",
        "member_change ann password set",
        "member_change olga deactivated",
        "login ann invite code",
        "login ann password",
    ] {
        assert!(
            details.iter().any(|d| d == expected),
            "{expected}: {details:#?}"
        );
    }
    assert!(
        details
            .iter()
            .any(|d| d
                .starts_with("settings_change smtp smtp smtp.corp.io:587 starttls, password set")),
        "{details:#?}"
    );
    assert!(!audit.to_string().contains("smtp-pass-123"));
}

/// The RFC 6238 code of base32 `secret` at time `step`, computed here on
/// its own (not with the kernel's code).
fn totp_at(secret: &str, step: i64) -> String {
    use hmac::{Hmac, Mac};
    let (mut bits, mut held, mut key) = (0u64, 0, Vec::new());
    for c in secret.bytes() {
        let value = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ234567"
            .iter()
            .position(|a| *a == c)
            .unwrap() as u64;
        bits = (bits << 5) | value;
        held += 5;
        if held >= 8 {
            held -= 8;
            key.push((bits >> held) as u8);
            bits &= (1 << held) - 1;
        }
    }
    let mut mac = Hmac::<sha1::Sha1>::new_from_slice(&key).unwrap();
    mac.update(&(step as u64).to_be_bytes());
    let hash = mac.finalize().into_bytes();
    let at = usize::from(hash[19] & 15);
    let code = u32::from_be_bytes([hash[at] & 0x7f, hash[at + 1], hash[at + 2], hash[at + 3]]);
    format!("{:06}", code % 1_000_000)
}

fn step_now() -> i64 {
    chrono::Utc::now().timestamp() / 30
}

/// `POST /session` with a right password that needs a second step: the
/// challenge and its methods (no cookie).
async fn challenge(app: &Router, email: &str, password: &str) -> (String, Value) {
    let body = json!({ "email": email, "password": password });
    let (status, headers, answer) =
        send(app, "POST", "/api/v1/session", Some(""), Some(body)).await;
    assert_eq!(status, StatusCode::OK, "{answer}");
    assert!(headers.get(header::SET_COOKIE).is_none());
    let two_step = &answer["two_step"];
    (
        two_step["challenge"].as_str().unwrap().to_owned(),
        two_step["methods"].clone(),
    )
}

/// `POST /session/two_step`: the session cookie, or the status.
async fn second_step(
    app: &Router,
    challenge: &str,
    method: &str,
    code: &str,
) -> Result<String, StatusCode> {
    let body = json!({ "challenge": challenge, "method": method, "code": code });
    let path = "/api/v1/session/two_step";
    let (status, headers, _) = send(app, "POST", path, Some(""), Some(body)).await;
    match status {
        StatusCode::NO_CONTENT => Ok(set_cookie(&headers, "sun_token")),
        status => Err(status),
    }
}

/// Mails the code for `challenge` and returns it.
async fn mailed_code(k: &common::Kernel, challenge: &str) -> String {
    let body = Some(json!({ "challenge": challenge }));
    let path = "/api/v1/session/two_step/email";
    let (status, _, _) = send(&k.app, "POST", path, Some(""), body).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    code_in(&k.mails.last())
}

/// An authenticator and emailed codes as the second step after a
/// password: enrolment, replays, the 5-miss challenge, the policies
/// (required, and for password changes), an admin's reset, and an emailed
/// reset code that skips it.
#[tokio::test]
async fn auth_two_step_sign_in_policies_and_reset() {
    let k = kernel().await;
    let app = &k.app;
    let smtp = json!({ "host": "smtp.corp.io", "port": 587, "username": "sun",
        "from": "sun@corp.io", "tls": "starttls", "password": "smtp-pass-123" });
    send(app, "PUT", "/api/v1/smtp", None, Some(smtp)).await;
    let person = |name: &str| {
        Some(
            json!({ "email": format!("{name}@corp.io"), "username": name,
            "role": "member", "password": format!("{name}'s long password"),
            "must_change": false }),
        )
    };
    let (status, _, bob_row) = send(app, "POST", "/api/v1/members", None, person("bob")).await;
    assert_eq!(status, StatusCode::CREATED);
    let bob_id = bob_row["id"].as_str().unwrap().to_owned();
    let bob_password = "bob's long password";
    let cookie = password_session(app, "bob@corp.io", bob_password)
        .await
        .unwrap();
    let bob = Some(cookie.as_str());

    // An authenticator counts once a code of it confirmed it.
    let label = Some(json!({ "label": "Phone" }));
    let (status, _, started) = send(app, "POST", "/api/v1/me/two_step/totp", bob, label).await;
    assert_eq!(status, StatusCode::OK, "{started}");
    let secret = started["secret"].as_str().unwrap().to_owned();
    let url = started["otpauth_url"].as_str().unwrap();
    assert!(
        url.starts_with(&format!(
            "otpauth://totp/Sun:bob%40corp.io?secret={secret}&"
        )),
        "{url}"
    );
    let qr = started["qr_svg"].as_str().unwrap();
    assert!(qr.starts_with("data:image/svg+xml;base64,"));
    let (_, _, mine) = send(app, "GET", "/api/v1/me/two_step", bob, None).await;
    assert_eq!(mine["factors"], json!([]));
    let now = step_now();
    let confirm = |code: String| Some(json!({ "id": started["id"], "code": code }));
    let path = "/api/v1/me/two_step/totp/confirm";
    let (status, _, _) = send(app, "POST", path, bob, confirm("000000".into())).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    let (status, _, factor) = send(app, "POST", path, bob, confirm(totp_at(&secret, now))).await;
    assert_eq!(status, StatusCode::OK, "{factor}");
    assert_eq!(
        (&factor["kind"], &factor["label"]),
        (&json!("totp"), &json!("Phone"))
    );
    let (_, _, members) = send(app, "GET", "/api/v1/members", None, None).await;
    let row = |id: &str| {
        members
            .as_array()
            .unwrap()
            .iter()
            .find(|m| m["id"] == id)
            .cloned()
    };
    assert_eq!(row(&bob_id).unwrap()["has_two_step"], true);

    // A right password now gets a challenge. The confirming code was used;
    // the next step's works once; email isn't one of his methods.
    let (c, methods) = challenge(app, "bob@corp.io", bob_password).await;
    assert_eq!(methods, json!(["totp"]));
    let replay = second_step(app, &c, "totp", &totp_at(&secret, now)).await;
    assert_eq!(replay, Err(StatusCode::UNAUTHORIZED));
    let body = Some(json!({ "challenge": c }));
    let (status, _, _) = send(
        app,
        "POST",
        "/api/v1/session/two_step/email",
        Some(""),
        body,
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let session = second_step(app, &c, "totp", &totp_at(&secret, now + 1))
        .await
        .unwrap();
    let (status, _, _) = send(app, "GET", "/api/v1/tasks", Some(&session), None).await;
    assert_eq!(status, StatusCode::OK);
    let again = second_step(app, &c, "totp", &totp_at(&secret, now + 1)).await;
    assert_eq!(
        again,
        Err(StatusCode::UNAUTHORIZED),
        "a challenge works once"
    );

    // Emailed codes too: the code goes to his mail; five misses end a
    // challenge, even for the right code after them.
    let (status, _, _) = send(app, "POST", "/api/v1/me/two_step/email", bob, None).await;
    assert_eq!(status, StatusCode::CREATED);
    let (status, _, _) = send(app, "POST", "/api/v1/me/two_step/email", bob, None).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "already on");
    let (c, methods) = challenge(app, "bob@corp.io", bob_password).await;
    assert_eq!(methods, json!(["totp", "email"]));
    let code = mailed_code(&k, &c).await;
    assert_eq!(k.mails.last().to, "bob@corp.io");
    second_step(app, &c, "email", &code).await.unwrap();
    let (c, _) = challenge(app, "bob@corp.io", bob_password).await;
    let code = mailed_code(&k, &c).await; // one try
    for _ in 0..4 {
        let wrong = second_step(app, &c, "email", "999999").await;
        assert_eq!(wrong, Err(StatusCode::UNAUTHORIZED));
    }
    assert_eq!(
        second_step(app, &c, "email", &code).await,
        Err(StatusCode::UNAUTHORIZED)
    );

    // Policies: managed with manage_settings only.
    let policy = Some(json!({ "require_two_step": true, "two_step_for_password_change": true }));
    let path = "/api/v1/settings/security";
    let (status, _, _) = send(app, "PUT", path, bob, policy.clone()).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, _, _) = send(app, "PUT", path, None, policy.clone()).await;
    assert_eq!(status, StatusCode::OK);
    let (_, _, saved) = send(app, "GET", path, None, None).await;
    assert_eq!(Some(saved), policy);

    // Required: carol, without a second step, proves her email, then must
    // add a second step before anything else.
    let (_, _, carol_row) = send(app, "POST", "/api/v1/members", None, person("carol")).await;
    let (c, methods) = challenge(app, "carol@corp.io", "carol's long password").await;
    assert_eq!(methods, json!(["email"]));
    let code = mailed_code(&k, &c).await;
    let carol_cookie = second_step(app, &c, "email", &code).await.unwrap();
    let carol = Some(carol_cookie.as_str());
    let (status, _, error) = send(app, "GET", "/api/v1/tasks", carol, None).await;
    assert_eq!(
        (status, &error["error"]["code"]),
        (StatusCode::FORBIDDEN, &json!("two_step_required"))
    );
    let (_, _, me) = send(app, "GET", "/api/v1/me", carol, None).await;
    assert_eq!(me["must_enroll_two_step"], true);
    let (status, _, only) = send(app, "POST", "/api/v1/me/two_step/email", carol, None).await;
    assert_eq!(status, StatusCode::CREATED);
    let remove_only = format!("/api/v1/me/two_step/{}", only["id"].as_str().unwrap());
    let (status, _, error) = send(app, "DELETE", &remove_only, carol, None).await;
    assert_eq!(
        (status, error["error"]["message"].as_str()),
        (
            StatusCode::CONFLICT,
            Some("Add another method before removing this one.")
        )
    );
    let (status, _, _) = send(app, "GET", "/api/v1/tasks", carol, None).await;
    assert_eq!(status, StatusCode::OK);

    // Changing a password needs the second step too.
    let change = |two_step: Option<Value>| {
        let mut body =
            json!({ "password": "bob's next password", "current_password": bob_password });
        if let Some(two_step) = two_step {
            body["two_step"] = two_step;
        }
        Some(body)
    };
    let (status, _, error) = send(app, "PUT", "/api/v1/me/password", bob, change(None)).await;
    assert_eq!(
        (status, &error["error"]["code"]),
        (StatusCode::FORBIDDEN, &json!("two_step_required"))
    );
    let (_, _, mine) = send(app, "POST", "/api/v1/me/two_step/challenge", bob, None).await;
    let c = mine["challenge"].as_str().unwrap().to_owned();
    let wrong = Some(json!({ "challenge": c, "method": "email", "code": "999999" }));
    let (status, _, _) = send(app, "PUT", "/api/v1/me/password", bob, change(wrong)).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let code = mailed_code(&k, &c).await;
    let right = Some(json!({ "challenge": c, "method": "email", "code": code }));
    let (status, _, _) = send(app, "PUT", "/api/v1/me/password", bob, change(right)).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    // He has two methods: one of them can go, even while 2-step is required.
    let remove = format!("/api/v1/me/two_step/{}", factor["id"].as_str().unwrap());
    let (status, _, _) = send(app, "DELETE", &remove, bob, None).await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    // An admin resets his second steps (he lost his phone); a member can't.
    let reset = format!("/api/v1/members/{bob_id}/two_step");
    let (status, _, _) = send(app, "DELETE", &reset, bob, None).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, _, member) = send(app, "DELETE", &reset, None, None).await;
    assert_eq!(
        (status, &member["has_two_step"]),
        (StatusCode::OK, &json!(false))
    );
    let (status, _, _) = send(app, "GET", "/api/v1/me", bob, None).await;
    assert_eq!(
        status,
        StatusCode::UNAUTHORIZED,
        "a reset ends his sessions"
    );
    let off = Some(json!({ "require_two_step": false, "two_step_for_password_change": false }));
    send(app, "PUT", "/api/v1/settings/security", None, off).await;
    password_session(app, "bob@corp.io", "bob's next password")
        .await
        .unwrap();

    // An emailed reset code proves the email: no second step after it.
    let carol_id = carol_row["id"].as_str().unwrap();
    let reinvite = format!("/api/v1/members/{carol_id}/invite");
    let (status, _, _) = send(app, "POST", &reinvite, None, None).await;
    assert_eq!(status, StatusCode::OK);
    let code = code_in(&k.mails.last());
    password_session(app, "carol@corp.io", &code).await.unwrap();

    let (_, _, audit) = send(app, "GET", "/api/v1/audit?limit=500", None, None).await;
    let audit = audit.to_string();
    for expected in [
        "2-step: authenticator added",
        "2-step: email codes on",
        "password + totp",
        "password + email",
        "2-step required: true, for password changes: true",
        "2-step reset by an admin",
    ] {
        assert!(audit.contains(expected), "{expected}: {audit}");
    }
    assert!(!audit.contains(&secret));
}

/// A request with `Authorization: Bearer <token>`: its status.
async fn bearer(app: &Router, token: &str, path: &str) -> StatusCode {
    let request = Request::builder()
        .uri(path)
        .header(header::AUTHORIZATION, format!("Bearer {token}"))
        .body(Body::empty())
        .unwrap();
    common::respond(app, request).await.status()
}

/// The first-run owner's first own password rotates the kernel token
/// (it was printed, maybe into logs): the old one is refused at once, the
/// new one works, and a later change keeps it.
#[tokio::test]
async fn auth_first_password_rotates_the_kernel_token() {
    let k = kernel().await;
    let calls = Arc::new(Mutex::new(0));
    let counter = calls.clone();
    let api = k.api.clone().with_token_rotation(Arc::new(move || {
        let mut calls = counter.lock().unwrap();
        *calls += 1;
        match *calls {
            1 => Err("disk full".to_owned()),
            _ => Ok("rotated-token".to_owned()),
        }
    }));
    let app = &sun_api::router(api.clone());
    api.seed_owner().await.unwrap();
    let cookie = password_session(app, "admin@sun.local", TOKEN)
        .await
        .unwrap();
    let me = Some(cookie.as_str());
    let (status, _, error) = send(app, "GET", "/api/v1/tasks", me, None).await;
    assert_eq!(
        (status, &error["error"]["code"]),
        (StatusCode::FORBIDDEN, &json!("password_required"))
    );
    let own = Some(json!({ "password": "the owner's own password" }));
    // A rotation that fails changes nothing: the old token and password stay.
    let (status, _, error) = send(app, "PUT", "/api/v1/me/password", me, own.clone()).await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR, "{error}");
    assert!(error.to_string().contains("disk full"), "{error}");
    assert_eq!(bearer(app, TOKEN, "/api/v1/tasks").await, StatusCode::OK);
    let (_, _, mine) = send(app, "GET", "/api/v1/me", me, None).await;
    assert_eq!(mine["must_set_password"], true);
    let (status, _, _) = send(app, "PUT", "/api/v1/me/password", me, own).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert_eq!(*calls.lock().unwrap(), 2);
    assert_eq!(
        bearer(app, TOKEN, "/api/v1/tasks").await,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        bearer(app, "rotated-token", "/api/v1/tasks").await,
        StatusCode::OK
    );
    assert_eq!(
        password_session(app, "admin@sun.local", TOKEN).await,
        Err(StatusCode::UNAUTHORIZED)
    );

    // Only once: the next change keeps the token.
    let email = Some(json!({ "email": "boss@corp.io" }));
    send(app, "PUT", "/api/v1/me/email", me, email).await;
    let next = Some(json!({ "password": "the owner's next password",
        "current_password": "the owner's own password" }));
    let (status, _, _) = send(app, "PUT", "/api/v1/me/password", me, next).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert_eq!(*calls.lock().unwrap(), 2);
    assert_eq!(
        bearer(app, "rotated-token", "/api/v1/tasks").await,
        StatusCode::OK
    );
    let audit = Request::builder()
        .uri("/api/v1/audit?limit=50")
        .header(header::AUTHORIZATION, "Bearer rotated-token")
        .body(Body::empty())
        .unwrap();
    let body = common::respond(app, audit)
        .await
        .into_body()
        .collect()
        .await;
    let audit = String::from_utf8(body.unwrap().to_bytes().to_vec()).unwrap();
    assert!(
        audit.contains("rotated: the first-run password was replaced"),
        "{audit}"
    );
}

/// With "require 2-step" on, a password session of a member who also
/// has an SSO identity is gated like any other; only the session an SSO
/// sign-in started skips Sun's 2-step.
#[tokio::test]
async fn auth_only_sso_sessions_skip_the_required_two_step() {
    let idp = mock_idp().await;
    let k = oidc_kernel(&idp, &[]).await;
    let app = &k.app;
    invite(&k, "dana", "corp.io", Role::Member).await;
    // Linking her SSO identity (an admin's password set below ends sessions).
    sign_in(app, &idp, "dana", "corp.io").await.1.unwrap();
    let (_, _, members) = send(app, "GET", "/api/v1/members", None, None).await;
    let dana = members.as_array().unwrap()[0]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let set = format!("/api/v1/members/{dana}/password");
    let body = Some(json!({ "password": "dana's long password", "must_change": false }));
    let (status, _, _) = send(app, "PUT", &set, None, body).await;
    assert_eq!(status, StatusCode::OK);
    let policy = Some(json!({ "require_two_step": true, "two_step_for_password_change": false }));
    send(app, "PUT", "/api/v1/settings/security", None, policy).await;

    let (_, sso_now) = sign_in(app, &idp, "dana", "corp.io").await;
    let sso_now = sso_now.unwrap();
    let (status, _, me) = send(app, "GET", "/api/v1/me", Some(&sso_now), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        (&me["via"], &me["must_enroll_two_step"]),
        (&json!("sso"), &json!(false))
    );
    let (status, _, _) = send(app, "GET", "/api/v1/tasks", Some(&sso_now), None).await;
    assert_eq!(status, StatusCode::OK);

    let password = password_session(app, "dana@corp.io", "dana's long password")
        .await
        .unwrap();
    let (_, _, me) = send(app, "GET", "/api/v1/me", Some(&password), None).await;
    assert_eq!(
        (&me["via"], &me["must_enroll_two_step"]),
        (&json!("local"), &json!(true))
    );
    let (status, _, error) = send(app, "GET", "/api/v1/tasks", Some(&password), None).await;
    assert_eq!(
        (status, &error["error"]["code"]),
        (StatusCode::FORBIDDEN, &json!("two_step_required"))
    );
}

/// A software passkey: one P-256 key that answers WebAuthn options the way a
/// browser and its authenticator do, as `PublicKeyCredential.toJSON()`.
/// `origin` and `rp_id` are where it believes it is (a phishing page signs
/// for its own).
struct SoftPasskey {
    key: p256::ecdsa::SigningKey,
    id: Vec<u8>,
    count: u32,
}

/// base64url without padding, as WebAuthn's JSON has it.
fn b64(bytes: &[u8]) -> String {
    use base64::Engine as _;
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

/// A CBOR byte string.
fn cbor_bytes(bytes: &[u8]) -> Vec<u8> {
    let head = match bytes.len() {
        n @ 0..24 => vec![0x40 | n as u8],
        n @ 24..256 => vec![0x58, n as u8],
        n => vec![0x59, (n >> 8) as u8, n as u8],
    };
    [head, bytes.to_vec()].concat()
}

impl SoftPasskey {
    fn new(seed: u8) -> Self {
        Self {
            key: p256::ecdsa::SigningKey::from_slice(&[seed; 32]).unwrap(),
            id: vec![seed; 16],
            count: 0,
        }
    }

    fn client_data(kind: &str, options: &Value, origin: &str, cross_origin: bool) -> Vec<u8> {
        json!({ "type": kind, "challenge": options["challenge"], "origin": origin,
            "crossOrigin": cross_origin })
        .to_string()
        .into_bytes()
    }

    /// `navigator.credentials.create` (attestation `none`).
    fn create(&self, options: &Value, origin: &str, rp_id: &str) -> Value {
        self.create_with(options, origin, rp_id, 0x45, "webauthn.create")
    }

    /// [`Self::create`] with these authenticator data `flags` and
    /// client data `kind` (a broken or lying authenticator).
    fn create_with(
        &self,
        options: &Value,
        origin: &str,
        rp_id: &str,
        flags: u8,
        kind: &str,
    ) -> Value {
        let point = self.key.verifying_key().to_encoded_point(false);
        // COSE EC2 key: {1: 2, 3: -7 (ES256), -1: 1 (P-256), -2: x, -3: y}.
        let cose = [
            vec![0xa5, 0x01, 0x02, 0x03, 0x26, 0x20, 0x01, 0x21],
            cbor_bytes(point.x().unwrap()),
            vec![0x22],
            cbor_bytes(point.y().unwrap()),
        ]
        .concat();
        let auth_data = [
            Sha256::digest(rp_id.as_bytes()).to_vec(),
            vec![flags], // 0x45: user present + verified, attested credential data
            self.count.to_be_bytes().to_vec(),
            vec![0; 16],
            (self.id.len() as u16).to_be_bytes().to_vec(),
            self.id.clone(),
            cose,
        ]
        .concat();
        let attestation = [
            b"\xa3\x63fmt\x64none\x67attStmt\xa0\x68authData".to_vec(),
            cbor_bytes(&auth_data),
        ]
        .concat();
        json!({ "id": b64(&self.id), "rawId": b64(&self.id), "type": "public-key",
            "response": {
                "clientDataJSON": b64(&Self::client_data(kind, options, origin, false)),
                "attestationObject": b64(&attestation),
                "transports": ["internal"],
            },
            "clientExtensionResults": {} })
    }

    /// `navigator.credentials.get`: signs `authData ‖ sha256(clientDataJSON)`.
    fn get(&mut self, options: &Value, origin: &str, rp_id: &str) -> Value {
        self.get_with(options, origin, rp_id, 0x05, "webauthn.get", false)
    }

    /// [`Self::get`] with these authenticator data `flags` (0x05: user
    /// present + verified), client data `kind` and `crossOrigin`, signed
    /// properly: what a broken, lying or framed authenticator sends.
    fn get_with(
        &mut self,
        options: &Value,
        origin: &str,
        rp_id: &str,
        flags: u8,
        kind: &str,
        cross_origin: bool,
    ) -> Value {
        use p256::ecdsa::signature::Signer;
        self.count += 1;
        let auth_data = [
            Sha256::digest(rp_id.as_bytes()).to_vec(),
            vec![flags],
            self.count.to_be_bytes().to_vec(),
        ]
        .concat();
        let client_data = Self::client_data(kind, options, origin, cross_origin);
        let signed = [auth_data.clone(), Sha256::digest(&client_data).to_vec()].concat();
        let signature: p256::ecdsa::Signature = self.key.sign(&signed);
        json!({ "id": b64(&self.id), "rawId": b64(&self.id), "type": "public-key",
            "response": {
                "clientDataJSON": b64(&client_data),
                "authenticatorData": b64(&auth_data),
                "signature": b64(signature.to_der().as_bytes()),
            },
            "clientExtensionResults": {} })
    }
}

/// `POST /session/two_step {challenge, method: "passkey"}`: the request
/// options for the challenge.
async fn passkey_options(app: &Router, challenge: &str) -> Value {
    let body = Some(json!({ "challenge": challenge, "method": "passkey" }));
    let (status, _, answer) = send(app, "POST", "/api/v1/session/two_step", Some(""), body).await;
    assert_eq!(status, StatusCode::OK, "{answer}");
    answer["options"].clone()
}

/// `POST /session/two_step` answered with a passkey `credential`.
async fn passkey_step(
    app: &Router,
    challenge: &str,
    credential: Value,
) -> Result<String, StatusCode> {
    let body = json!({ "challenge": challenge, "method": "passkey", "credential": credential });
    let path = "/api/v1/session/two_step";
    let (status, headers, _) = send(app, "POST", path, Some(""), Some(body)).await;
    match status {
        StatusCode::NO_CONTENT => Ok(set_cookie(&headers, "sun_token")),
        status => Err(status),
    }
}

/// A passkey as the second step, with a software authenticator.
/// Adding one and signing in with it; a wrong origin or relying party, a
/// stranger's key, a replayed challenge or a stale signature is refused; it
/// counts for "require 2-step"; asking for passkey options keeps a mailed
/// code; it confirms a password change.
#[tokio::test]
async fn auth_passkey_register_sign_in_and_refusals() {
    const ORIGIN: &str = "http://localhost";
    const RP: &str = "localhost";
    let k = kernel().await;
    let app = &k.app;
    let smtp = json!({ "host": "smtp.corp.io", "port": 587, "username": "sun",
        "from": "sun@corp.io", "tls": "starttls", "password": "smtp-pass-123" });
    send(app, "PUT", "/api/v1/smtp", None, Some(smtp)).await;
    let password = "eve's long password";
    let body = json!({ "email": "eve@corp.io", "username": "eve",
        "role": "member", "password": password, "must_change": false });
    let (status, _, _) = send(app, "POST", "/api/v1/members", None, Some(body)).await;
    assert_eq!(status, StatusCode::CREATED);
    let cookie = password_session(app, "eve@corp.io", password)
        .await
        .unwrap();
    let eve = Some(cookie.as_str());
    let policy = Some(json!({ "require_two_step": true, "two_step_for_password_change": true }));
    send(app, "PUT", "/api/v1/settings/security", None, policy).await;

    // Gated until she has a second step, she may still add a passkey.
    let (status, _, _) = send(app, "GET", "/api/v1/tasks", eve, None).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (_, _, mine) = send(app, "GET", "/api/v1/me/two_step", eve, None).await;
    assert_eq!(mine["passkey_ready"], true);
    let start = "/api/v1/me/two_step/passkey";
    let (status, _, started) = send(app, "POST", start, eve, None).await;
    assert_eq!(status, StatusCode::OK, "{started}");
    let options = &started["options"];
    assert_eq!(options["rp"]["id"], RP);
    assert_eq!(options["user"]["name"], "eve@corp.io");
    assert_eq!(options["attestation"], "none");
    let mut passkey = SoftPasskey::new(3);
    let confirm = |credential: Value| {
        Some(
            json!({ "challenge": started["challenge"], "credential": credential,
            "label": "Laptop" }),
        )
    };
    let path = "/api/v1/me/two_step/passkey/confirm";
    for (origin, rp) in [("http://evil.example", RP), (ORIGIN, "evil.example")] {
        let wrong = confirm(passkey.create(options, origin, rp));
        let (status, _, _) = send(app, "POST", path, eve, wrong).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{origin} {rp}");
    }
    let right = confirm(passkey.create(options, ORIGIN, RP));
    let (status, _, factor) = send(app, "POST", path, eve, right.clone()).await;
    assert_eq!(status, StatusCode::CREATED, "{factor}");
    assert_eq!(
        (&factor["kind"], &factor["label"]),
        (&json!("passkey"), &json!("Laptop"))
    );
    let (status, _, _) = send(app, "POST", path, eve, right).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "a used challenge");
    let (status, _, _) = send(app, "GET", "/api/v1/tasks", eve, None).await;
    assert_eq!(status, StatusCode::OK);
    // It is her only method while 2-step is required: it stays.
    let remove = format!("/api/v1/me/two_step/{}", factor["id"].as_str().unwrap());
    let (status, _, _) = send(app, "DELETE", &remove, eve, None).await;
    assert_eq!(status, StatusCode::CONFLICT);
    // The token holder has no passkeys to add.
    let (status, _, _) = send(app, "POST", start, None, None).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);

    // Signing in: options for her passkey, then its signature.
    let (c, methods) = challenge(app, "eve@corp.io", password).await;
    assert_eq!(methods, json!(["passkey"]));
    let options = passkey_options(app, &c).await;
    assert_eq!(options["rpId"], RP);
    assert_eq!(options["allowCredentials"][0]["id"], b64(&passkey.id));
    let refused = Err(StatusCode::UNAUTHORIZED);
    let wrong_origin = passkey.get(&options, "http://evil.example", RP);
    assert_eq!(passkey_step(app, &c, wrong_origin).await, refused);
    let wrong_rp = passkey.get(&options, ORIGIN, "evil.example");
    assert_eq!(passkey_step(app, &c, wrong_rp).await, refused);
    let stranger = SoftPasskey::new(4).get(&options, ORIGIN, RP);
    assert_eq!(passkey_step(app, &c, stranger).await, refused);
    let signed = passkey.get(&options, ORIGIN, RP);
    let session = passkey_step(app, &c, signed.clone()).await.unwrap();
    let (status, _, _) = send(app, "GET", "/api/v1/tasks", Some(&session), None).await;
    assert_eq!(status, StatusCode::OK);
    // The challenge was used; on a new one, the old signature is stale.
    assert_eq!(passkey_step(app, &c, signed.clone()).await, refused);
    let (c, _) = challenge(app, "eve@corp.io", password).await;
    passkey_options(app, &c).await;
    assert_eq!(passkey_step(app, &c, signed).await, refused);

    // With email codes too: a mailed code survives asking for passkey options.
    let (status, _, _) = send(app, "POST", "/api/v1/me/two_step/email", eve, None).await;
    assert_eq!(status, StatusCode::CREATED);
    let (c, methods) = challenge(app, "eve@corp.io", password).await;
    assert_eq!(methods, json!(["passkey", "email"]));
    let code = mailed_code(&k, &c).await;
    passkey_options(app, &c).await;
    second_step(app, &c, "email", &code).await.unwrap();

    // A password change, confirmed with the passkey.
    let path = "/api/v1/me/two_step/challenge";
    let (status, _, mine) = send(app, "POST", path, eve, None).await;
    assert_eq!(status, StatusCode::OK, "{mine}");
    let c = mine["challenge"].as_str().unwrap();
    let options = passkey_options(app, c).await;
    let change = json!({ "password": "eve's new long password", "current_password": password,
        "two_step": { "challenge": c, "method": "passkey",
            "credential": passkey.get(&options, ORIGIN, RP) } });
    let (status, _, error) = send(app, "PUT", "/api/v1/me/password", eve, Some(change)).await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{error}");
}

// ---------- Edge, abuse and permission cases ----------

/// `POST path` from client address `ip` (the sign-in limiter's key).
async fn post_from(
    app: &Router,
    ip: [u8; 4],
    path: &str,
    body: Value,
) -> (StatusCode, HeaderMap, Value) {
    let mut request = Request::builder()
        .method("POST")
        .uri(path)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(body.to_string()))
        .unwrap();
    let addr = std::net::SocketAddr::from((ip, 40_000));
    request
        .extensions_mut()
        .insert(axum::extract::ConnectInfo(addr));
    let response = common::respond(app, request).await;
    let (status, headers) = (response.status(), response.headers().clone());
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, headers, value)
}

/// Saves SMTP settings as the token holder.
async fn smtp_on(app: &Router) {
    let smtp = json!({ "host": "smtp.corp.io", "port": 587, "username": "sun",
        "from": "sun@corp.io", "tls": "starttls", "password": "smtp-pass-123" });
    let (status, _, _) = send(app, "PUT", "/api/v1/smtp", None, Some(smtp)).await;
    assert_eq!(status, StatusCode::OK);
}

/// The password [`person`] gives `name`.
fn password_of(name: &str) -> String {
    format!("{name}'s long password")
}

/// The token holder adds `name@corp.io` as `role` in Main with
/// [`password_of`] and no change asked: their id.
async fn person(app: &Router, name: &str, role: Role, is_owner: bool) -> String {
    let body = json!({ "email": format!("{name}@corp.io"), "username": name,
        "role": role, "password": password_of(name), "must_change": false,
        "is_owner": is_owner });
    let (status, _, member) = send(app, "POST", "/api/v1/members", None, Some(body)).await;
    assert_eq!(status, StatusCode::CREATED, "{member}");
    member["id"].as_str().unwrap().to_owned()
}

/// Adds an authenticator for the session `cookie`, confirmed with the
/// previous time step's code (so this step and the next are still unused):
/// its base32 secret.
async fn enroll_totp(app: &Router, cookie: &str) -> String {
    let path = "/api/v1/me/two_step/totp";
    let (status, _, started) = send(app, "POST", path, Some(cookie), Some(json!({}))).await;
    assert_eq!(status, StatusCode::OK, "{started}");
    let secret = started["secret"].as_str().unwrap().to_owned();
    let body = json!({ "id": started["id"], "code": totp_at(&secret, step_now() - 1) });
    let path = "/api/v1/me/two_step/totp/confirm";
    let (status, _, factor) = send(app, "POST", path, Some(cookie), Some(body)).await;
    assert_eq!(status, StatusCode::OK, "{factor}");
    secret
}

/// Waits for a fresh 30 s time step when this one is half gone, so codes a
/// test computes for `step_now() ± n` mean the same step to the kernel.
async fn settle() {
    let into = chrono::Utc::now().timestamp() % 30;
    if into >= 15 {
        tokio::time::sleep(Duration::from_secs((31 - into) as u64)).await;
    }
}

/// Every sign-in failure is one answer: unknown email, wrong password, a
/// deactivated member's right password or live code, wrong, used and
/// expired codes, a malformed email, and an unknown 2-step challenge or
/// method. Nothing tells an attacker which part was wrong.
#[tokio::test]
async fn auth_every_sign_in_failure_reads_the_same() {
    let k = kernel().await;
    let app = &k.app;
    smtp_on(app).await;
    person(app, "bob", Role::Member, false).await;
    let dee = person(app, "dee", Role::Member, false).await;
    let deactivate = format!("/api/v1/members/{dee}/deactivate");
    let (status, _, _) = send(app, "POST", &deactivate, None, None).await;
    assert_eq!(status, StatusCode::OK);
    let ann = json!({ "email": "ann@corp.io", "username": "ann" });
    let (status, _, ann) = send(app, "POST", "/api/v1/members", None, Some(ann)).await;
    assert_eq!(status, StatusCode::CREATED, "{ann}");
    let ann: sun_core::MemberId = ann["id"].as_str().unwrap().parse().unwrap();
    let code = code_in(&k.mails.last());
    let miss = if code == "000000" { "111111" } else { "000000" };
    // A live code of the deactivated dee (stored after deactivating).
    let later = chrono::Utc::now() + chrono::Duration::minutes(10);
    let mut tx = k.store.begin().await.unwrap();
    let dee_id: sun_core::MemberId = dee.parse().unwrap();
    tx.insert_otp(dee_id, &sha256_hex("dee@corp.io:654321"), later)
        .await
        .unwrap();
    tx.commit().await.unwrap();

    let session =
        |email: &str, password: &str| Some(json!({ "email": email, "password": password }));
    let wrong_password = session("bob@corp.io", "not bob's password");
    let (status, _, wrong) = send(app, "POST", "/api/v1/session", Some(""), wrong_password).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(
        wrong["error"]["message"],
        "That email, password or code is not right."
    );
    let same = |what: &str, (status, _, body): (StatusCode, HeaderMap, Value)| {
        assert_eq!(
            (status, &body),
            (StatusCode::UNAUTHORIZED, &wrong),
            "{what}"
        );
    };
    for (what, email, password) in [
        (
            "unknown email",
            "nobody@corp.io",
            "whatever password".to_owned(),
        ),
        ("empty password", "bob@corp.io", String::new()),
        (
            "deactivated, right password",
            "dee@corp.io",
            password_of("dee"),
        ),
        ("deactivated, live code", "dee@corp.io", "654321".to_owned()),
        ("wrong code", "ann@corp.io", miss.to_owned()),
        ("not an email", "ann at corp.io", code.clone()),
    ] {
        let body = session(email, &password);
        same(
            what,
            send(app, "POST", "/api/v1/session", Some(""), body).await,
        );
    }
    // Used once, a code is spent; an expired one never worked.
    let (status, _, _) = send(
        app,
        "POST",
        "/api/v1/session",
        Some(""),
        session("ann@corp.io", &code),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let body = session("ann@corp.io", &code);
    same(
        "used code",
        send(app, "POST", "/api/v1/session", Some(""), body).await,
    );
    let past = chrono::Utc::now() - chrono::Duration::seconds(1);
    let mut tx = k.store.begin().await.unwrap();
    tx.insert_otp(ann, &sha256_hex("ann@corp.io:123123"), past)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    let body = session("ann@corp.io", "123123");
    same(
        "expired code",
        send(app, "POST", "/api/v1/session", Some(""), body).await,
    );
    // The second step answers the same.
    for (what, method) in [("unknown challenge", "totp"), ("unknown method", "sms")] {
        let body = json!({ "challenge": "no-such-challenge", "method": method, "code": "123456" });
        let path = "/api/v1/session/two_step";
        same(what, send(app, "POST", path, Some(""), Some(body)).await);
    }
}

/// Ten failures per email and client address within the window, password
/// and 2-step misses alike, then `429` even for the right answers; another
/// address isn't held back.
#[tokio::test]
async fn auth_rate_limit_counts_two_step_misses_per_email_and_ip() {
    let k = kernel().await;
    let app = &k.app;
    person(app, "bob", Role::Member, false).await;
    let password = password_of("bob");
    let cookie = password_session(app, "bob@corp.io", &password)
        .await
        .unwrap();
    let secret = enroll_totp(app, &cookie).await;
    let (a, b) = ([10, 0, 0, 1], [10, 0, 0, 2]);
    let sign_in = |password: &str| json!({ "email": "bob@corp.io", "password": password });
    for _ in 0..5 {
        let body = sign_in("not bob's password");
        let (status, _, _) = post_from(app, a, "/api/v1/session", body).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
    }
    // The right password gets a challenge, whose misses count on.
    let (status, _, answer) = post_from(app, a, "/api/v1/session", sign_in(&password)).await;
    assert_eq!(status, StatusCode::OK, "{answer}");
    let c = answer["two_step"]["challenge"].clone();
    for _ in 0..5 {
        let body = json!({ "challenge": c, "method": "totp", "code": "000000" });
        let (status, _, _) = post_from(app, a, "/api/v1/session/two_step", body).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
    }
    let (status, _, limited) = post_from(app, a, "/api/v1/session", sign_in(&password)).await;
    assert_eq!(
        (status, &limited["error"]["code"]),
        (StatusCode::TOO_MANY_REQUESTS, &json!("rate_limited"))
    );
    // From another address the password works; its challenge can't be
    // answered from the held-back one, and is from its own.
    let (status, _, answer) = post_from(app, b, "/api/v1/session", sign_in(&password)).await;
    assert_eq!(status, StatusCode::OK, "{answer}");
    let c = answer["two_step"]["challenge"].clone();
    let right = json!({ "challenge": c, "method": "totp", "code": totp_at(&secret, step_now()) });
    let path = "/api/v1/session/two_step";
    let (status, _, _) = post_from(app, a, path, right.clone()).await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
    let (status, _, _) = post_from(app, b, path, right).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
}

/// The limiter must hold when the tries come at once, not one by one.
#[tokio::test]
async fn auth_rate_limit_holds_under_parallel_tries() {
    let k = kernel().await;
    person(&k.app, "bob", Role::Member, false).await;
    let mut tries = tokio::task::JoinSet::new();
    for _ in 0..15 {
        let app = k.app.clone();
        tries.spawn(async move {
            let body = json!({ "email": "bob@corp.io", "password": "not bob's password" });
            send(&app, "POST", "/api/v1/session", Some(""), Some(body))
                .await
                .0
        });
    }
    let mut checked = 0;
    while let Some(status) = tries.join_next().await {
        if status.unwrap() == StatusCode::UNAUTHORIZED {
            checked += 1;
        }
    }
    assert!(checked <= 10, "{checked} of 15 parallel tries were checked");
}

/// Wrong `current_password` tries on `PUT /me/password` are limited per
/// member and IP like sign-ins; a right one clears the count.
#[tokio::test]
async fn auth_wrong_current_password_is_rate_limited() {
    let k = kernel().await;
    let app = &k.app;
    person(app, "bob", Role::Member, false).await;
    let cookie = password_session(app, "bob@corp.io", &password_of("bob"))
        .await
        .unwrap();
    let bob = Some(cookie.as_str());
    let change =
        |current: &str, next: &str| Some(json!({ "password": next, "current_password": current }));
    let path = "/api/v1/me/password";
    for _ in 0..9 {
        let wrong = change("not bob's password", "bob's next password");
        assert_eq!(
            send(app, "PUT", path, bob, wrong).await.0,
            StatusCode::FORBIDDEN
        );
    }
    let right = change(&password_of("bob"), "bob's next password");
    assert_eq!(
        send(app, "PUT", path, bob, right).await.0,
        StatusCode::NO_CONTENT
    );
    for _ in 0..10 {
        let wrong = change("not bob's password", "bob's third password");
        assert_eq!(
            send(app, "PUT", path, bob, wrong).await.0,
            StatusCode::FORBIDDEN
        );
    }
    let right = change("bob's next password", "bob's third password");
    let (status, _, error) = send(app, "PUT", path, bob, right).await;
    assert_eq!(
        (status, &error["error"]["code"]),
        (StatusCode::TOO_MANY_REQUESTS, &json!("rate_limited"))
    );
}

/// `PUT /settings/security` changes only the rules it is given.
#[tokio::test]
async fn auth_security_settings_keep_rules_left_out() {
    let k = kernel().await;
    let app = &k.app;
    let path = "/api/v1/settings/security";
    let put = |body: Value| send(app, "PUT", path, None, Some(body));
    let (status, _, saved) = put(json!({ "require_two_step": true })).await;
    assert_eq!(status, StatusCode::OK);
    let both = json!({ "require_two_step": true, "two_step_for_password_change": true });
    let (_, _, saved_too) = put(json!({ "two_step_for_password_change": true })).await;
    assert_eq!(
        (saved, saved_too),
        (
            json!({ "require_two_step": true, "two_step_for_password_change": false }),
            both.clone()
        )
    );
    assert_eq!(put(json!({})).await.2, both);
    let (_, _, off) = put(json!({ "require_two_step": false })).await;
    assert_eq!(off["two_step_for_password_change"], true);
    assert_eq!(send(app, "GET", path, None, None).await.2, off);
    let (status, _, _) = put(json!({ "require_2fa": true })).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
}

/// Codes are what they are for, and only while they live: a mailed 2-step
/// code isn't a sign-in code, an invite or reset code past its 15 minutes
/// and a challenge past its 5 minutes are refused (every route), and a code
/// within its time works.
#[tokio::test]
async fn auth_codes_expire_and_never_cross_over() {
    let k = kernel().await;
    let app = &k.app;
    smtp_on(app).await;
    let bob = person(app, "bob", Role::Member, false).await;
    let password = password_of("bob");
    let cookie = password_session(app, "bob@corp.io", &password)
        .await
        .unwrap();
    let path = "/api/v1/me/two_step/email";
    let (status, _, _) = send(app, "POST", path, Some(&cookie), None).await;
    assert_eq!(status, StatusCode::CREATED);
    let (c, _) = challenge(app, "bob@corp.io", &password).await;
    let code = mailed_code(&k, &c).await;
    assert_eq!(
        password_session(app, "bob@corp.io", &code).await,
        Err(StatusCode::UNAUTHORIZED),
        "a 2-step code in the password box"
    );
    second_step(app, &c, "email", &code).await.unwrap();

    let id: sun_core::MemberId = bob.parse().unwrap();
    let past = chrono::Utc::now() - chrono::Duration::seconds(1);
    let mut tx = k.store.begin().await.unwrap();
    tx.insert_otp(id, &sha256_hex("bob@corp.io:123123"), past)
        .await
        .unwrap();
    tx.insert_two_step(&sha256_hex("old-challenge"), id, past)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    assert_eq!(
        password_session(app, "bob@corp.io", "123123").await,
        Err(StatusCode::UNAUTHORIZED),
        "an expired reset code"
    );
    let body = Some(json!({ "challenge": "old-challenge" }));
    let path = "/api/v1/session/two_step/email";
    let (status, _, _) = send(app, "POST", path, Some(""), body).await;
    assert_eq!(
        status,
        StatusCode::UNAUTHORIZED,
        "mail for an expired challenge"
    );
    assert_eq!(
        second_step(app, "old-challenge", "email", "123123").await,
        Err(StatusCode::UNAUTHORIZED)
    );
    let body = Some(json!({ "challenge": "old-challenge", "method": "passkey" }));
    let (status, _, _) = send(app, "POST", "/api/v1/session/two_step", Some(""), body).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    // A reset code with a minute left signs in (and skips the second step).
    let later = chrono::Utc::now() + chrono::Duration::minutes(1);
    let mut tx = k.store.begin().await.unwrap();
    tx.insert_otp(id, &sha256_hex("bob@corp.io:456456"), later)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    password_session(app, "bob@corp.io", "456456")
        .await
        .unwrap();
}

/// Deactivating someone mid sign-in kills their sessions and their open
/// challenges (answer, mail and passkey routes); reactivating doesn't bring
/// those back, but a new sign-in works.
#[tokio::test]
async fn auth_deactivation_ends_sessions_and_challenges_until_reactivated() {
    let k = kernel().await;
    let app = &k.app;
    smtp_on(app).await;
    let bob = person(app, "bob", Role::Member, false).await;
    let password = password_of("bob");
    let cookie = password_session(app, "bob@corp.io", &password)
        .await
        .unwrap();
    let secret = enroll_totp(app, &cookie).await;
    let path = "/api/v1/me/two_step/email";
    let (status, _, _) = send(app, "POST", path, Some(&cookie), None).await;
    assert_eq!(status, StatusCode::CREATED);
    let session = second_step(
        app,
        &challenge(app, "bob@corp.io", &password).await.0,
        "totp",
        &totp_at(&secret, step_now()),
    )
    .await
    .unwrap();
    let (mailed, _) = challenge(app, "bob@corp.io", &password).await;
    let code = mailed_code(&k, &mailed).await;
    let (open, _) = challenge(app, "bob@corp.io", &password).await;

    let deactivate = format!("/api/v1/members/{bob}/deactivate");
    let (status, _, _) = send(app, "POST", &deactivate, None, None).await;
    assert_eq!(status, StatusCode::OK);
    let refused = Err(StatusCode::UNAUTHORIZED);
    for cookie in [&cookie, &session] {
        let (status, _, _) = send(app, "GET", "/api/v1/me", Some(cookie), None).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
    }
    assert_eq!(second_step(app, &mailed, "email", &code).await, refused);
    let next = totp_at(&secret, step_now() + 1);
    assert_eq!(second_step(app, &open, "totp", &next).await, refused);
    let body = Some(json!({ "challenge": open }));
    let (status, _, _) = send(
        app,
        "POST",
        "/api/v1/session/two_step/email",
        Some(""),
        body,
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let body = Some(json!({ "challenge": open, "method": "passkey" }));
    let (status, _, _) = send(app, "POST", "/api/v1/session/two_step", Some(""), body).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(
        password_session(app, "bob@corp.io", &password).await,
        refused
    );

    let activate = format!("/api/v1/members/{bob}/activate");
    let (status, _, _) = send(app, "POST", &activate, None, None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(second_step(app, &open, "totp", &next).await, refused);
    let (status, _, _) = send(app, "GET", "/api/v1/me", Some(&session), None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let (c, _) = challenge(app, "bob@corp.io", &password).await;
    second_step(app, &c, "totp", &next).await.unwrap();
}

/// RFC 6238 at the edges: ±1 step and nothing further, each step once per
/// authenticator (also an older step of the window after a newer one),
/// and anything but six ASCII digits is no code. Starting an authenticator
/// again replaces the one being added.
#[tokio::test]
async fn auth_totp_window_replays_and_junk_codes() {
    let k = kernel().await;
    let app = &k.app;
    person(app, "bob", Role::Member, false).await;
    let password = password_of("bob");
    let cookie = password_session(app, "bob@corp.io", &password)
        .await
        .unwrap();
    let me = Some(cookie.as_str());
    settle().await;
    let start = "/api/v1/me/two_step/totp";
    let (_, _, first) = send(app, "POST", start, me, Some(json!({}))).await;
    let (_, _, again) = send(app, "POST", start, me, Some(json!({}))).await;
    let code = totp_at(first["secret"].as_str().unwrap(), step_now());
    let body = Some(json!({ "id": first["id"], "code": code }));
    let confirm = "/api/v1/me/two_step/totp/confirm";
    let (status, _, _) = send(app, "POST", confirm, me, body).await;
    assert_eq!(
        status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "replaced by the second start"
    );
    let secret = again["secret"].as_str().unwrap().to_owned();
    let body = Some(json!({ "id": again["id"], "code": totp_at(&secret, step_now() - 1) }));
    let (status, _, _) = send(app, "POST", confirm, me, body).await;
    assert_eq!(status, StatusCode::OK);
    let now = step_now();
    let refused = Err(StatusCode::UNAUTHORIZED);

    let (c, _) = challenge(app, "bob@corp.io", &password).await;
    for junk in ["", "12345", "1234567", "12345a"] {
        assert_eq!(
            second_step(app, &c, "totp", junk).await,
            refused,
            "{junk:?}"
        );
    }
    let (c, _) = challenge(app, "bob@corp.io", &password).await;
    for junk in ["１２３４５６", "-12345"] {
        assert_eq!(
            second_step(app, &c, "totp", junk).await,
            refused,
            "{junk:?}"
        );
    }
    for step in [now + 2, now - 2] {
        let code = totp_at(&secret, step);
        assert_eq!(
            second_step(app, &c, "totp", &code).await,
            refused,
            "step {}",
            step - now
        );
    }
    second_step(app, &c, "totp", &format!(" {} ", totp_at(&secret, now + 1)))
        .await
        .unwrap();
    let (c, _) = challenge(app, "bob@corp.io", &password).await;
    for step in [now, now + 1] {
        let code = totp_at(&secret, step);
        assert_eq!(
            second_step(app, &c, "totp", &code).await,
            refused,
            "step {} after step +1",
            step - now
        );
    }
}

/// A miss with one method keeps the other's mailed code; five misses end
/// the challenge (the right code and a mail after them fail); two answers
/// at once to one challenge, or the same authenticator code on two
/// challenges at once, start one session.
#[tokio::test]
async fn auth_two_step_lockout_switching_and_one_session_per_answer() {
    let k = kernel().await;
    let app = &k.app;
    smtp_on(app).await;
    person(app, "bob", Role::Member, false).await;
    let password = password_of("bob");
    let cookie = password_session(app, "bob@corp.io", &password)
        .await
        .unwrap();
    let secret = enroll_totp(app, &cookie).await;
    let path = "/api/v1/me/two_step/email";
    let (status, _, _) = send(app, "POST", path, Some(&cookie), None).await;
    assert_eq!(status, StatusCode::CREATED);
    let refused = Err(StatusCode::UNAUTHORIZED);

    let (c, methods) = challenge(app, "bob@corp.io", &password).await;
    assert_eq!(methods, json!(["totp", "email"]));
    let code = mailed_code(&k, &c).await;
    assert_eq!(second_step(app, &c, "totp", "000000").await, refused);
    second_step(app, &c, "email", &code).await.unwrap();

    let (c, _) = challenge(app, "bob@corp.io", &password).await;
    for _ in 0..5 {
        assert_eq!(second_step(app, &c, "totp", "000000").await, refused);
    }
    let right = totp_at(&secret, step_now());
    assert_eq!(second_step(app, &c, "totp", &right).await, refused);
    let body = Some(json!({ "challenge": c }));
    let (status, _, _) = send(
        app,
        "POST",
        "/api/v1/session/two_step/email",
        Some(""),
        body,
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    let (c, _) = challenge(app, "bob@corp.io", &password).await;
    let code = mailed_code(&k, &c).await;
    let (one, two) = tokio::join!(
        second_step(app, &c, "email", &code),
        second_step(app, &c, "email", &code)
    );
    assert_eq!(
        usize::from(one.is_ok()) + usize::from(two.is_ok()),
        1,
        "{one:?} {two:?}"
    );
    let (c1, _) = challenge(app, "bob@corp.io", &password).await;
    let (c2, _) = challenge(app, "bob@corp.io", &password).await;
    let code = totp_at(&secret, step_now());
    let (one, two) = tokio::join!(
        second_step(app, &c1, "totp", &code),
        second_step(app, &c2, "totp", &code)
    );
    assert_eq!(
        usize::from(one.is_ok()) + usize::from(two.is_ok()),
        1,
        "{one:?} {two:?}"
    );
    let (_, _, audit) = send(app, "GET", "/api/v1/audit?limit=500", None, None).await;
    let logins = audit
        .as_array()
        .unwrap()
        .iter()
        .filter(|e| {
            e["action"] == "login" && e["detail"].as_str().unwrap().starts_with("password + ")
        })
        .count();
    assert_eq!(logins, 3, "{audit}");
}

/// While a required second step is missing, only `GET /me` and adding a
/// second step get through (plus signing out); everything else, the
/// member's other own routes included, is `403 two_step_required`.
#[tokio::test]
async fn auth_required_two_step_gate_opens_only_enrolment() {
    let k = kernel().await;
    let app = &k.app;
    // An Admin, so only the gate stands in the way. No SMTP: no emailed
    // challenge, the session starts and is gated.
    person(app, "bob", Role::Admin, false).await;
    let policy = Some(json!({ "require_two_step": true }));
    let (status, _, _) = send(app, "PUT", "/api/v1/settings/security", None, policy).await;
    assert_eq!(status, StatusCode::OK);
    let cookie = password_session(app, "bob@corp.io", &password_of("bob"))
        .await
        .unwrap();
    let me = Some(cookie.as_str());
    let (_, _, mine) = send(app, "GET", "/api/v1/me", me, None).await;
    assert_eq!(mine["must_enroll_two_step"], true, "{mine}");
    let factor = sun_core::FactorId::generate();
    let blocked = [
        ("GET", "/api/v1/tasks".to_owned(), None),
        ("GET", "/api/v1/workspaces".into(), None),
        ("GET", "/api/v1/members".into(), None),
        ("GET", "/api/v1/audit".into(), None),
        ("GET", "/api/v1/settings/security".into(), None),
        (
            "PUT",
            "/api/v1/settings/security".into(),
            Some(json!({ "require_two_step": false })),
        ),
        (
            "POST",
            "/api/v1/members".into(),
            Some(json!({ "email": "x@corp.io", "username": "x" })),
        ),
        (
            "PUT",
            "/api/v1/me/view_role".into(),
            Some(json!({ "view_role": null })),
        ),
        (
            "PUT",
            "/api/v1/me/password".into(),
            Some(json!({ "password": "bob's next password",
                "current_password": password_of("bob") })),
        ),
        ("POST", "/api/v1/me/two_step/challenge".into(), None),
        ("DELETE", format!("/api/v1/me/two_step/{factor}"), None),
    ];
    for (method, path, body) in blocked {
        let (status, _, error) = send(app, method, &path, me, body).await;
        assert_eq!(
            (status, &error["error"]["code"]),
            (StatusCode::FORBIDDEN, &json!("two_step_required")),
            "{method} {path}"
        );
    }
    for (method, path) in [
        ("GET", "/api/v1/me"),
        ("GET", "/api/v1/me/two_step"),
        ("POST", "/api/v1/me/two_step/passkey"),
    ] {
        let (status, _, body) = send(app, method, path, me, None).await;
        assert_eq!(status, StatusCode::OK, "{method} {path}: {body}");
    }
    enroll_totp(app, &cookie).await;
    let (status, _, _) = send(app, "GET", "/api/v1/tasks", me, None).await;
    assert_eq!(status, StatusCode::OK);
    let (status, _, _) = send(app, "DELETE", "/api/v1/session", me, None).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (status, _, _) = send(app, "GET", "/api/v1/me", me, None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

/// Two owners deactivating each other at the same moment: one wins, the
/// other is refused, and one active owner always stays (the token holder
/// can't take that one either).
#[tokio::test]
async fn auth_two_owners_deactivating_each_other_keep_one() {
    let k = kernel().await;
    let app = &k.app;
    let olive = person(app, "olive", Role::Viewer, true).await;
    let otto = person(app, "otto", Role::Viewer, true).await;
    let o1 = password_session(app, "olive@corp.io", &password_of("olive"))
        .await
        .unwrap();
    let o2 = password_session(app, "otto@corp.io", &password_of("otto"))
        .await
        .unwrap();
    let (by_olive, by_otto) = (
        format!("/api/v1/members/{otto}/deactivate"),
        format!("/api/v1/members/{olive}/deactivate"),
    );
    let (a, b) = tokio::join!(
        send(app, "POST", &by_olive, Some(&o1), None),
        send(app, "POST", &by_otto, Some(&o2), None)
    );
    let statuses = [a.0, b.0];
    assert_eq!(
        statuses.iter().filter(|s| **s == StatusCode::OK).count(),
        1,
        "{a:?} {b:?}"
    );
    assert!(
        statuses.iter().all(|s| [
            StatusCode::OK,
            StatusCode::UNAUTHORIZED,
            StatusCode::UNPROCESSABLE_ENTITY
        ]
        .contains(s)),
        "{a:?} {b:?}"
    );
    let (_, _, members) = send(app, "GET", "/api/v1/members", None, None).await;
    let active: Vec<&str> = members
        .as_array()
        .unwrap()
        .iter()
        .filter(|m| m["is_owner"] == true && m["deactivated_at"].is_null())
        .map(|m| m["id"].as_str().unwrap())
        .collect();
    assert_eq!(active.len(), 1, "{members}");
    let last = format!("/api/v1/members/{}/deactivate", active[0]);
    let (status, _, _) = send(app, "POST", &last, None, None).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
}

/// Only an owner touches an owner, on every member route: an Admin who is
/// no owner gets `403` for an owner's password, reset code, deactivation,
/// reactivation, 2-step reset and owner flag, and for making a new owner;
/// the owner is untouched.
#[tokio::test]
async fn auth_a_non_owner_admin_never_touches_an_owner() {
    let k = kernel().await;
    let app = &k.app;
    smtp_on(app).await;
    let olive = person(app, "olive", Role::Admin, true).await;
    let oc = password_session(app, "olive@corp.io", &password_of("olive"))
        .await
        .unwrap();
    let (status, _, _) = send(app, "POST", "/api/v1/me/two_step/email", Some(&oc), None).await;
    assert_eq!(status, StatusCode::CREATED);
    person(app, "ada", Role::Admin, false).await;
    let ada = password_session(app, "ada@corp.io", &password_of("ada"))
        .await
        .unwrap();
    let at = |what: &str| format!("/api/v1/members/{olive}/{what}");
    let probes = [
        (
            "PUT",
            at("password"),
            Some(json!({ "password": "ada picked this one" })),
        ),
        ("POST", at("invite"), None),
        ("POST", at("deactivate"), None),
        ("POST", at("activate"), None),
        ("DELETE", at("two_step"), None),
        ("PUT", at("owner"), Some(json!({ "is_owner": false }))),
        (
            "POST",
            "/api/v1/members".into(),
            Some(
                json!({ "email": "o2@corp.io", "username": "o2", "is_owner": true,
                "password": "a second owner's password" }),
            ),
        ),
    ];
    for (method, path, body) in probes {
        let (status, _, error) = send(app, method, &path, Some(&ada), body).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{method} {path}: {error}");
    }
    let (_, _, me) = send(app, "GET", "/api/v1/me", Some(&oc), None).await;
    assert_eq!(me["is_owner"], true, "{me}");
    let (_, _, mine) = send(app, "GET", "/api/v1/me/two_step", Some(&oc), None).await;
    assert_eq!(mine["factors"].as_array().unwrap().len(), 1, "{mine}");
}

/// A password an admin sets (at creation or later) never comes back: not
/// in the answer, the member list, `/me` or the audit log, nor its hash.
#[tokio::test]
async fn auth_admin_set_passwords_never_come_back() {
    let k = kernel().await;
    let app = &k.app;
    let (first, second) = ("the first secret password", "the second secret password");
    let clean = |what: &str, value: &Value| {
        let text = value.to_string();
        for leak in [first, second, "argon2", "password_hash"] {
            assert!(!text.contains(leak), "{what} shows {leak}: {text}");
        }
    };
    let body = json!({ "email": "bob@corp.io", "username": "bob", "password": first });
    let (status, _, created) = send(app, "POST", "/api/v1/members", None, Some(body)).await;
    assert_eq!(status, StatusCode::CREATED);
    clean("POST /members", &created);
    let set = format!(
        "/api/v1/members/{}/password",
        created["id"].as_str().unwrap()
    );
    let (status, _, member) =
        send(app, "PUT", &set, None, Some(json!({ "password": second }))).await;
    assert_eq!(status, StatusCode::OK);
    clean("PUT /members/{id}/password", &member);
    let cookie = password_session(app, "bob@corp.io", second).await.unwrap();
    let (_, _, me) = send(app, "GET", "/api/v1/me", Some(&cookie), None).await;
    assert_eq!(me["must_set_password"], true);
    clean("/me", &me);
    let (_, _, members) = send(app, "GET", "/api/v1/members", None, None).await;
    clean("GET /members", &members);
    let (_, _, audit) = send(app, "GET", "/api/v1/audit?limit=500", None, None).await;
    clean("GET /audit", &audit);
}

/// An admin-set password to change, for someone with an authenticator and
/// the "2-step for password changes" policy: the sign-in still asks for the
/// authenticator, then only for a new password (no current one, no second
/// 2-step). Someone without a method, under "require 2-step", proves their
/// email, sets a password, then must add a method.
#[tokio::test]
async fn auth_must_change_comes_after_the_second_step_and_before_enrolment() {
    let k = kernel().await;
    let app = &k.app;
    smtp_on(app).await;
    let bob = person(app, "bob", Role::Member, false).await;
    let carol = person(app, "carol", Role::Member, false).await;
    let cookie = password_session(app, "bob@corp.io", &password_of("bob"))
        .await
        .unwrap();
    let secret = enroll_totp(app, &cookie).await;
    let policy = Some(json!({ "require_two_step": true, "two_step_for_password_change": true }));
    send(app, "PUT", "/api/v1/settings/security", None, policy).await;
    for id in [&bob, &carol] {
        let set = format!("/api/v1/members/{id}/password");
        let body = Some(json!({ "password": "the admin picked this" }));
        let (status, _, _) = send(app, "PUT", &set, None, body).await;
        assert_eq!(status, StatusCode::OK);
    }
    let code_of = |error: &Value| error["error"]["code"].clone();

    let (c, methods) = challenge(app, "bob@corp.io", "the admin picked this").await;
    assert_eq!(methods, json!(["totp"]));
    let bob = second_step(app, &c, "totp", &totp_at(&secret, step_now()))
        .await
        .unwrap();
    let (status, _, error) = send(app, "GET", "/api/v1/tasks", Some(&bob), None).await;
    assert_eq!(
        (status, code_of(&error)),
        (StatusCode::FORBIDDEN, json!("password_required"))
    );
    let own = Some(json!({ "password": "bob's very own password" }));
    let (status, _, error) = send(app, "PUT", "/api/v1/me/password", Some(&bob), own).await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{error}");
    let (status, _, _) = send(app, "GET", "/api/v1/tasks", Some(&bob), None).await;
    assert_eq!(status, StatusCode::OK);

    let (c, methods) = challenge(app, "carol@corp.io", "the admin picked this").await;
    assert_eq!(methods, json!(["email"]));
    let code = mailed_code(&k, &c).await;
    let carol = second_step(app, &c, "email", &code).await.unwrap();
    let (status, _, error) = send(app, "GET", "/api/v1/tasks", Some(&carol), None).await;
    assert_eq!(
        (status, code_of(&error)),
        (StatusCode::FORBIDDEN, json!("password_required"))
    );
    let own = Some(json!({ "password": "carol's very own password" }));
    let (status, _, _) = send(app, "PUT", "/api/v1/me/password", Some(&carol), own).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (status, _, error) = send(app, "GET", "/api/v1/tasks", Some(&carol), None).await;
    assert_eq!(
        (status, code_of(&error)),
        (StatusCode::FORBIDDEN, json!("two_step_required"))
    );
}

/// After the rotation, the new token is no password either, and the next
/// start neither re-seeds the owner nor makes them change their own
/// password again.
#[tokio::test]
async fn auth_rotated_token_is_no_password_and_a_restart_keeps_it() {
    let k = kernel().await;
    let api = k
        .api
        .clone()
        .with_token_rotation(Arc::new(|| Ok("rotated-token-2".to_owned())));
    let app = &sun_api::router(api.clone());
    api.seed_owner().await.unwrap();
    let cookie = password_session(app, "admin@sun.local", TOKEN)
        .await
        .unwrap();
    let own = Some(json!({ "password": "the owner's own password" }));
    let (status, _, _) = send(app, "PUT", "/api/v1/me/password", Some(&cookie), own).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    for password in [TOKEN, "rotated-token-2"] {
        assert_eq!(
            password_session(app, "admin@sun.local", password).await,
            Err(StatusCode::UNAUTHORIZED),
            "{password}"
        );
    }
    assert_eq!(api.seed_owner().await.unwrap(), None);
    let (_, _, me) = send(app, "GET", "/api/v1/me", Some(&cookie), None).await;
    assert_eq!(
        (&me["must_set_password"], &me["needs_email"]),
        (&json!(false), &json!(true)),
        "{me}"
    );
    password_session(app, "admin@sun.local", "the owner's own password")
        .await
        .unwrap();
}

/// Only an email the provider verified, and for GitHub only the primary
/// one, links an invited member; with allowed domains an unverified email
/// is denied. A callback without its code or state starts no session.
#[tokio::test]
async fn auth_sso_unverified_or_secondary_emails_never_link() {
    let idp = mock_idp().await;
    let k = oidc_kernel(&idp, &[]).await;
    let closed = invite_only(&k);
    let app = &closed;
    invite(&k, "alice", "corp.io", Role::Viewer).await;
    let (landing, session) =
        sign_in_with(app, &idp, "mock", "sub-alice", "alice@corp.io", true, "").await;
    assert_eq!((landing.as_str(), session), ("/#/login/invite", None));
    assert_eq!(identity_of(&k, "alice@corp.io").await.0, "local");

    // The mock's GitHub also lists someone-else@corp.io: verified, not primary.
    add_provider(app, &idp, "gh", "github", &[]).await;
    invite(&k, "someone-else", "corp.io", Role::Viewer).await;
    let (landing, session) = sign_in_with(app, &idp, "gh", "777", "carol@corp.io", false, "").await;
    assert_eq!((landing.as_str(), session), ("/#/login/invite", None));
    assert_eq!(identity_of(&k, "someone-else@corp.io").await.0, "local");

    add_provider(app, &idp, "mock", "oidc", &["corp.io"]).await;
    let (landing, session) =
        sign_in_with(app, &idp, "mock", "sub-alice", "alice@corp.io", true, "").await;
    assert_eq!((landing.as_str(), session), ("/#/login/denied", None));

    for query in [
        "",
        "?code=good-code",
        "?state=x",
        "?error=access_denied&state=x",
    ] {
        let path = format!("/api/v1/auth/sso/mock/callback{query}");
        let (status, headers, _) = send(app, "GET", &path, Some("sun_sso_state=x"), None).await;
        assert_eq!(
            (status, location(&headers).as_str()),
            (StatusCode::SEE_OTHER, "/#/login/failed"),
            "{query}"
        );
        let session = headers
            .get_all(header::SET_COOKIE)
            .iter()
            .any(|v| v.to_str().unwrap().starts_with("sun_token="));
        assert!(!session, "{query}");
    }
}

/// Settings → Sign-in checks every field of a provider and keeps at most
/// 20; the client secret shows up nowhere (settings, login page, secrets
/// list, audit log).
#[tokio::test]
async fn auth_sso_provider_settings_are_checked_and_hide_the_secret() {
    let k = kernel().await;
    let app = &k.app;
    let ok = json!({ "kind": "oidc", "name": "Keycloak", "client_id": "sun",
        "issuer_url": "https://sso.corp.io/realms/x", "client_secret": "kc-secret-value" });
    let put = |id: String, body: Value| async move {
        let path = format!("/api/v1/settings/sso/{id}");
        send(app, "PUT", &path, None, Some(body)).await.0
    };
    for id in ["1abc", "a-b", "_x", "Abc", "a.b", "a%20b", &"a".repeat(33)] {
        assert_eq!(
            put(id.to_owned(), ok.clone()).await,
            StatusCode::UNPROCESSABLE_ENTITY,
            "{id}"
        );
    }
    let many: Vec<String> = (0..51).map(|i| format!("d{i}.io")).collect();
    for (field, value) in [
        ("name", json!("")),
        ("name", json!("x".repeat(65))),
        ("client_id", json!(" ")),
        ("kind", json!("saml")),
        ("issuer_url", json!("javascript:alert(1)")),
        ("issuer_url", json!("ftp://sso.corp.io")),
        ("issuer_url", json!("https://sso corp.io")),
        ("allowed_domains", json!(["corp"])),
        ("allowed_domains", json!(["a b.io"])),
        ("allowed_domains", json!(many)),
        ("client_secret", json!("   ")),
        ("extra", json!(1)),
    ] {
        let mut body = ok.clone();
        body[field] = value;
        assert_eq!(
            put("kc".into(), body).await,
            StatusCode::UNPROCESSABLE_ENTITY,
            "{field}"
        );
    }
    for i in 0..20 {
        assert_eq!(put(format!("p{i}"), ok.clone()).await, StatusCode::OK);
    }
    assert_eq!(
        put("p20".into(), ok.clone()).await,
        StatusCode::UNPROCESSABLE_ENTITY,
        "21 providers"
    );
    for path in [
        "/api/v1/settings/sso",
        "/api/v1/auth",
        "/api/v1/secrets",
        "/api/v1/audit?limit=500",
    ] {
        let (status, _, body) = send(app, "GET", path, None, None).await;
        assert_eq!(status, StatusCode::OK, "{path}");
        assert!(
            !body.to_string().contains("kc-secret-value"),
            "{path}: {body}"
        );
    }
}

/// Auth0 (and other providers) put a trailing slash in their issuer; the
/// saved `issuer_url` must keep it, or discovery finds another issuer.
#[tokio::test]
async fn auth_sso_issuer_with_a_trailing_slash_signs_in() {
    let idp = mock_idp_with("/").await;
    let k = kernel_with(Some(idp.presets())).await;
    add_provider(&k.app, &idp, "auth0", "oidc", &[]).await;
    let path = "/api/v1/auth/sso/auth0/login";
    let (status, headers, body) = send(&k.app, "GET", path, Some(""), None).await;
    assert_eq!(status, StatusCode::SEE_OTHER, "{body}");
    assert!(location(&headers).starts_with(&idp.issuer));
}

/// Registers `key` as a passkey of the member signed in with `cookie`.
async fn add_passkey(app: &Router, cookie: &str, key: &SoftPasskey) -> (StatusCode, Value) {
    let (status, _, started) = send(
        app,
        "POST",
        "/api/v1/me/two_step/passkey",
        Some(cookie),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{started}");
    let body = json!({ "challenge": started["challenge"],
        "credential": key.create(&started["options"], "http://localhost", "localhost") });
    let path = "/api/v1/me/two_step/passkey/confirm";
    let (status, _, factor) = send(app, "POST", path, Some(cookie), Some(body)).await;
    (status, factor)
}

/// Answers `challenge` with passkey `credential` from client address
/// `10.9.0.<n>` (a new one each time keeps the limiter out of the way).
async fn passkey_from(app: &Router, n: &mut u8, challenge: &str, credential: Value) -> StatusCode {
    *n += 1;
    let body = json!({ "challenge": challenge, "method": "passkey", "credential": credential });
    post_from(app, [10, 9, 0, *n], "/api/v1/session/two_step", body)
        .await
        .0
}

/// `credential` with `response.<field>` decoded, changed and encoded again.
fn tampered(credential: &Value, field: &str, change: impl FnOnce(&mut Vec<u8>)) -> Value {
    use base64::Engine as _;
    let mut credential = credential.clone();
    let text = credential["response"][field].as_str().unwrap();
    let mut bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(text)
        .unwrap();
    change(&mut bytes);
    credential["response"][field] = json!(b64(&bytes));
    credential
}

/// Adversarial passkey answers, each refused: tampered client data,
/// authenticator data or signature; a registration's client data type; no
/// user presence; another challenge's options; another member's passkey; a
/// sign count that doesn't grow; malformed and oversized JSON; an answer
/// before any options. Registration refuses no presence, the wrong type
/// and someone else's ceremony. The right answers still work in between.
#[tokio::test]
async fn auth_passkey_tampered_replayed_and_malformed_answers_fail() {
    const ORIGIN: &str = "http://localhost";
    const RP: &str = "localhost";
    let k = kernel().await;
    let app = &k.app;
    person(app, "eve", Role::Member, false).await;
    person(app, "max", Role::Member, false).await;
    let eve_pw = password_of("eve");
    let eve_cookie = password_session(app, "eve@corp.io", &eve_pw).await.unwrap();
    let max_cookie = password_session(app, "max@corp.io", &password_of("max"))
        .await
        .unwrap();
    let mut eve = SoftPasskey::new(5);
    let mut max = SoftPasskey::new(6);
    assert_eq!(
        add_passkey(app, &eve_cookie, &eve).await.0,
        StatusCode::CREATED
    );
    assert_eq!(
        add_passkey(app, &max_cookie, &max).await.0,
        StatusCode::CREATED
    );
    let refused = StatusCode::UNAUTHORIZED;
    let mut n = 0;

    let (c, _) = challenge(app, "eve@corp.io", &eve_pw).await;
    let options = passkey_options(app, &c).await;
    let signed = eve.get(&options, ORIGIN, RP);
    let attempts = [
        (
            "client data",
            tampered(&signed, "clientDataJSON", |b| b.insert(b.len() - 1, b' ')),
        ),
        (
            "authenticator data",
            tampered(&signed, "authenticatorData", |b| {
                *b.last_mut().unwrap() ^= 1
            }),
        ),
        (
            "signature",
            tampered(&signed, "signature", |b| *b.last_mut().unwrap() ^= 1),
        ),
        (
            "create type",
            eve.get_with(&options, ORIGIN, RP, 0x05, "webauthn.create", false),
        ),
    ];
    for (what, credential) in attempts {
        assert_eq!(
            passkey_from(app, &mut n, &c, credential).await,
            refused,
            "{what}"
        );
    }
    assert_eq!(
        passkey_from(app, &mut n, &c, signed).await,
        StatusCode::NO_CONTENT
    );

    let (c, _) = challenge(app, "eve@corp.io", &eve_pw).await;
    let options = passkey_options(app, &c).await;
    let (other, _) = challenge(app, "eve@corp.io", &eve_pw).await;
    let other_options = passkey_options(app, &other).await;
    let stale = {
        let mut copy = SoftPasskey::new(5);
        copy.get(&options, ORIGIN, RP) // count 1: no higher than the stored 1
    };
    let attempts = [
        (
            "no user presence",
            eve.get_with(&options, ORIGIN, RP, 0x04, "webauthn.get", false),
        ),
        ("another challenge", eve.get(&other_options, ORIGIN, RP)),
        ("max's passkey", max.get(&options, ORIGIN, RP)),
        ("sign count", stale),
    ];
    for (what, credential) in attempts {
        assert_eq!(
            passkey_from(app, &mut n, &c, credential).await,
            refused,
            "{what}"
        );
    }
    let right = eve.get(&options, ORIGIN, RP);
    assert_eq!(
        passkey_from(app, &mut n, &c, right).await,
        StatusCode::NO_CONTENT
    );

    let (c, _) = challenge(app, "eve@corp.io", &eve_pw).await;
    let options = passkey_options(app, &c).await;
    for junk in [
        json!("junk"),
        json!({}),
        json!({ "id": b64(&eve.id) }),
        json!({ "id": b64(&eve.id), "response": { "clientDataJSON": 5 } }),
    ] {
        assert_eq!(
            passkey_from(app, &mut n, &c, junk.clone()).await,
            refused,
            "{junk}"
        );
    }
    let (c, _) = challenge(app, "eve@corp.io", &eve_pw).await;
    let huge = json!({ "id": "A".repeat(1_500_000), "response": {} });
    let status = passkey_from(app, &mut n, &c, huge).await;
    assert!(
        !status.is_success() && !status.is_server_error(),
        "{status}"
    );
    // No options asked for this challenge: no ceremony to answer.
    let (c, _) = challenge(app, "eve@corp.io", &eve_pw).await;
    let unasked = eve.get(&options, ORIGIN, RP);
    assert_eq!(passkey_from(app, &mut n, &c, unasked).await, refused);

    // Registration: presence, type and whose ceremony are checked.
    let start = "/api/v1/me/two_step/passkey";
    let confirm = "/api/v1/me/two_step/passkey/confirm";
    let new = SoftPasskey::new(7);
    for (flags, kind) in [(0x44, "webauthn.create"), (0x45, "webauthn.get")] {
        let (_, _, started) = send(app, "POST", start, Some(&eve_cookie), None).await;
        let credential = new.create_with(&started["options"], ORIGIN, RP, flags, kind);
        let body = Some(json!({ "challenge": started["challenge"], "credential": credential }));
        let (status, _, _) = send(app, "POST", confirm, Some(&eve_cookie), body).await;
        assert_eq!(
            status,
            StatusCode::UNPROCESSABLE_ENTITY,
            "{flags:#x} {kind}"
        );
    }
    let (_, _, started) = send(app, "POST", start, Some(&eve_cookie), None).await;
    let credential = new.create(&started["options"], ORIGIN, RP);
    let body = Some(json!({ "challenge": started["challenge"], "credential": credential }));
    let (status, _, _) = send(app, "POST", confirm, Some(&max_cookie), body).await;
    assert_eq!(
        status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "max finishing eve's ceremony"
    );
}

/// WebAuthn's `crossOrigin: true` means Sun's page ran framed in another
/// site; Sun never frames itself, so such an assertion shouldn't count.
#[tokio::test]
async fn auth_passkey_answer_from_a_cross_origin_frame_is_refused() {
    let k = kernel().await;
    let app = &k.app;
    person(app, "eve", Role::Member, false).await;
    let cookie = password_session(app, "eve@corp.io", &password_of("eve"))
        .await
        .unwrap();
    let mut eve = SoftPasskey::new(5);
    assert_eq!(add_passkey(app, &cookie, &eve).await.0, StatusCode::CREATED);
    let (c, _) = challenge(app, "eve@corp.io", &password_of("eve")).await;
    let options = passkey_options(app, &c).await;
    let framed = eve.get_with(
        &options,
        "http://localhost",
        "localhost",
        0x05,
        "webauthn.get",
        true,
    );
    assert_eq!(
        passkey_step(app, &c, framed).await,
        Err(StatusCode::UNAUTHORIZED)
    );
}

/// A credential id belongs to one passkey (WebAuthn §7.1 step 25): adding
/// the same authenticator credential again, or as another member, is refused.
#[tokio::test]
async fn auth_passkey_credential_registers_once() {
    let k = kernel().await;
    let app = &k.app;
    person(app, "eve", Role::Member, false).await;
    person(app, "max", Role::Member, false).await;
    let eve = password_session(app, "eve@corp.io", &password_of("eve"))
        .await
        .unwrap();
    let max = password_session(app, "max@corp.io", &password_of("max"))
        .await
        .unwrap();
    let key = SoftPasskey::new(5);
    assert_eq!(add_passkey(app, &eve, &key).await.0, StatusCode::CREATED);
    let (status, factor) = add_passkey(app, &eve, &key).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "again: {factor}");
    let (status, factor) = add_passkey(app, &max, &key).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "as max: {factor}");
}

// ---------- B4f2 sign-in hardening, adversarial (test/b4t2) ----------

/// A right password that still needs its second step gives its try back,
/// so it never locks anyone out, but keeps the failures before it: nine
/// wrong passwords and one wrong 2-step code are the ten.
#[tokio::test]
async fn auth_right_password_before_two_step_gives_its_try_back() {
    let k = kernel().await;
    let app = &k.app;
    person(app, "bob", Role::Member, false).await;
    let password = password_of("bob");
    let cookie = password_session(app, "bob@corp.io", &password)
        .await
        .unwrap();
    enroll_totp(app, &cookie).await;
    let a = [10, 0, 0, 7];
    let sign_in = |password: &str| json!({ "email": "bob@corp.io", "password": password });
    for i in 0..15 {
        let (status, _, answer) = post_from(app, a, "/api/v1/session", sign_in(&password)).await;
        assert_eq!(status, StatusCode::OK, "right password #{i}: {answer}");
    }
    for _ in 0..9 {
        let body = sign_in("not bob's password");
        let (status, _, _) = post_from(app, a, "/api/v1/session", body).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
    }
    let (status, _, answer) = post_from(app, a, "/api/v1/session", sign_in(&password)).await;
    assert_eq!(status, StatusCode::OK, "the ninth failure still lets in");
    let c = &answer["two_step"]["challenge"];
    let miss = json!({ "challenge": c, "method": "totp", "code": "000000" });
    let (status, _, _) = post_from(app, a, "/api/v1/session/two_step", miss).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let (status, _, limited) = post_from(app, a, "/api/v1/session", sign_in(&password)).await;
    assert_eq!(
        (status, &limited["error"]["code"]),
        (StatusCode::TOO_MANY_REQUESTS, &json!("rate_limited"))
    );
}

/// Sign-in tries (per email) and `current_password` tries (per member)
/// count apart: using up one leaves the other, and another member, free.
#[tokio::test]
async fn auth_sign_in_and_password_change_limits_count_apart() {
    let k = kernel().await;
    let app = &k.app;
    person(app, "bob", Role::Member, false).await;
    person(app, "cat", Role::Member, false).await;
    let cookie = password_session(app, "bob@corp.io", &password_of("bob"))
        .await
        .unwrap();
    for _ in 0..10 {
        let wrong = password_session(app, "bob@corp.io", "not bob's password").await;
        assert_eq!(wrong, Err(StatusCode::UNAUTHORIZED));
    }
    let right = password_session(app, "bob@corp.io", &password_of("bob")).await;
    assert_eq!(right, Err(StatusCode::TOO_MANY_REQUESTS));

    let change = |cookie: &str, current: &str, next: &str| {
        let body = json!({ "password": next, "current_password": current });
        let cookie = cookie.to_owned();
        async move {
            send(app, "PUT", "/api/v1/me/password", Some(&cookie), Some(body))
                .await
                .0
        }
    };
    let second = "bob's second password";
    let status = change(&cookie, &password_of("bob"), second).await;
    assert_eq!(status, StatusCode::NO_CONTENT, "sign-in tries are apart");
    for _ in 0..10 {
        let status = change(&cookie, "not bob's password", "bob's third password").await;
        assert_eq!(status, StatusCode::FORBIDDEN);
    }
    let status = change(&cookie, second, "bob's third password").await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);

    let cat = password_session(app, "cat@corp.io", &password_of("cat"))
        .await
        .unwrap();
    let status = change(&cat, &password_of("cat"), "cat's second password").await;
    assert_eq!(status, StatusCode::NO_CONTENT, "another member");
}

/// With `two_step_for_password_change`, `PUT /me/password` is a 2-step
/// oracle for someone who holds the session and the password: each new
/// challenge gives fresh tries. Its misses should count like sign-in's
/// (ten, then `429`).
#[tokio::test]
async fn auth_two_step_misses_on_password_change_are_rate_limited() {
    let k = kernel().await;
    let app = &k.app;
    person(app, "bob", Role::Member, false).await;
    let password = password_of("bob");
    let cookie = password_session(app, "bob@corp.io", &password)
        .await
        .unwrap();
    let secret = enroll_totp(app, &cookie).await;
    let rule = json!({ "two_step_for_password_change": true });
    let (status, _, _) = send(app, "PUT", "/api/v1/settings/security", None, Some(rule)).await;
    assert_eq!(status, StatusCode::OK);
    settle().await;
    let bob = Some(cookie.as_str());
    let change = |challenge: &Value, code: String| {
        Some(
            json!({ "password": "bob's next password", "current_password": password,
            "two_step": { "challenge": challenge, "method": "totp", "code": code } }),
        )
    };
    for i in 0..10 {
        let path = "/api/v1/me/two_step/challenge";
        let (status, _, started) = send(app, "POST", path, bob, None).await;
        assert_eq!(status, StatusCode::OK, "{started}");
        let body = change(&started["challenge"], "000000".into());
        let (status, _, _) = send(app, "PUT", "/api/v1/me/password", bob, body).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "miss #{i}");
    }
    let (_, _, started) = send(app, "POST", "/api/v1/me/two_step/challenge", bob, None).await;
    let body = change(&started["challenge"], totp_at(&secret, step_now()));
    let (status, _, _) = send(app, "PUT", "/api/v1/me/password", bob, body).await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
}

/// The admin routes on your own account would skip what `/me/password`
/// asks: with `two_step_for_password_change` on, a member with
/// `edit_members` (or an owner) could reset their own password, reset code
/// or 2-step without the current password or the second step, so a stolen
/// session takes the account over. Self-service belongs to `/me/*`.
#[tokio::test]
async fn auth_admin_password_route_on_yourself_is_refused() {
    let k = kernel().await;
    let app = &k.app;
    let eda = person(app, "eda", Role::Admin, false).await;
    let cookie = password_session(app, "eda@corp.io", &password_of("eda"))
        .await
        .unwrap();
    enroll_totp(app, &cookie).await;
    let rule = json!({ "two_step_for_password_change": true });
    let (status, _, _) = send(app, "PUT", "/api/v1/settings/security", None, Some(rule)).await;
    assert_eq!(status, StatusCode::OK);
    let me = Some(cookie.as_str());
    let body =
        json!({ "password": "the thief's password", "current_password": password_of("eda") });
    let (status, _, error) = send(app, "PUT", "/api/v1/me/password", me, Some(body)).await;
    assert_eq!(
        (status, &error["error"]["code"]),
        (StatusCode::FORBIDDEN, &json!("two_step_required"))
    );
    let own = person(app, "own", Role::Viewer, true).await;
    let owner = password_session(app, "own@corp.io", &password_of("own"))
        .await
        .unwrap();
    let body = json!({ "password": "the thief's password", "must_change": false });
    for (cookie, id) in [(&cookie, &eda), (&owner, &own)] {
        for (method, route, body) in [
            ("PUT", "password", Some(body.clone())),
            ("POST", "invite", None),
            ("DELETE", "two_step", None),
        ] {
            let path = format!("/api/v1/members/{id}/{route}");
            let (status, _, error) = send(app, method, &path, Some(cookie.as_str()), body).await;
            assert_eq!(status, StatusCode::FORBIDDEN, "{method} {route}: {error}");
        }
    }
    let (_, _, mine) = send(app, "GET", "/api/v1/me/two_step", me, None).await;
    assert_eq!(mine["factors"].as_array().unwrap().len(), 1, "2-step kept");
    let still = password_session(app, "eda@corp.io", &password_of("eda")).await;
    assert_eq!(still, Err(StatusCode::OK), "password kept");
}

/// `crossOrigin: true` in a registration's client data (Sun's page framed
/// by another site) is refused like an assertion's, and leaves nothing
/// behind: the same authenticator still registers from Sun's own page.
#[tokio::test]
async fn auth_passkey_registration_from_a_cross_origin_frame_is_refused() {
    let k = kernel().await;
    let app = &k.app;
    person(app, "eve", Role::Member, false).await;
    let cookie = password_session(app, "eve@corp.io", &password_of("eve"))
        .await
        .unwrap();
    let eve = Some(cookie.as_str());
    let key = SoftPasskey::new(7);
    let (_, _, started) = send(app, "POST", "/api/v1/me/two_step/passkey", eve, None).await;
    let credential = key.create(&started["options"], "http://localhost", "localhost");
    let framed = tampered(&credential, "clientDataJSON", |bytes| {
        let mut data: Value = serde_json::from_slice(bytes).unwrap();
        data["crossOrigin"] = json!(true);
        *bytes = data.to_string().into_bytes();
    });
    let body = json!({ "challenge": started["challenge"], "credential": framed });
    let path = "/api/v1/me/two_step/passkey/confirm";
    let (status, _, error) = send(app, "POST", path, eve, Some(body)).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{error}");
    let (_, _, mine) = send(app, "GET", "/api/v1/me/two_step", eve, None).await;
    assert_eq!(mine["factors"], json!([]));
    assert_eq!(add_passkey(app, &cookie, &key).await.0, StatusCode::CREATED);
}

/// A credential id is taken whatever key comes with it, and only while
/// its passkey exists: once eve removes hers, max may add that id.
#[tokio::test]
async fn auth_passkey_credential_id_is_unique_whatever_its_key() {
    let k = kernel().await;
    let app = &k.app;
    person(app, "eve", Role::Member, false).await;
    person(app, "max", Role::Member, false).await;
    let eve = password_session(app, "eve@corp.io", &password_of("eve"))
        .await
        .unwrap();
    let max = password_session(app, "max@corp.io", &password_of("max"))
        .await
        .unwrap();
    let (status, factor) = add_passkey(app, &eve, &SoftPasskey::new(5)).await;
    assert_eq!(status, StatusCode::CREATED);
    let lookalike = SoftPasskey {
        key: p256::ecdsa::SigningKey::from_slice(&[6; 32]).unwrap(),
        id: vec![5; 16],
        count: 0,
    };
    let (status, _) = add_passkey(app, &max, &lookalike).await;
    assert_eq!(
        status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "eve's id, max's key"
    );
    let path = format!("/api/v1/me/two_step/{}", factor["id"].as_str().unwrap());
    let (status, _, _) = send(app, "DELETE", &path, Some(&eve), None).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (status, factor) = add_passkey(app, &max, &lookalike).await;
    assert_eq!(status, StatusCode::CREATED, "free again: {factor}");
}

/// Two members finishing the same credential at once: the uniqueness check
/// is a read before the insert, so both may pass it. At most one passkey
/// may come out of each race.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn auth_passkey_same_credential_added_at_once_registers_once() {
    let k = kernel().await;
    let app = &k.app;
    person(app, "eve", Role::Member, false).await;
    person(app, "max", Role::Member, false).await;
    let mut cookies = Vec::new();
    for name in ["eve", "max"] {
        let email = format!("{name}@corp.io");
        cookies.push(
            password_session(app, &email, &password_of(name))
                .await
                .unwrap(),
        );
    }
    for seed in 20..30 {
        let key = SoftPasskey::new(seed);
        let mut races = tokio::task::JoinSet::new();
        for cookie in &cookies {
            let path = "/api/v1/me/two_step/passkey";
            let (_, _, started) = send(app, "POST", path, Some(cookie), None).await;
            let credential = key.create(&started["options"], "http://localhost", "localhost");
            let body = json!({ "challenge": started["challenge"], "credential": credential });
            let (app, cookie) = (app.clone(), cookie.clone());
            races.spawn(async move {
                let path = "/api/v1/me/two_step/passkey/confirm";
                send(&app, "POST", path, Some(&cookie), Some(body)).await.0
            });
        }
        let created = races
            .join_all()
            .await
            .into_iter()
            .filter(|s| *s == StatusCode::CREATED)
            .count();
        assert!(
            created <= 1,
            "seed {seed}: {created} passkeys of one credential"
        );
    }
}

/// A provider without a secret and one that doesn't exist answer the same
/// on the login and callback routes: nothing tells which ids are set up.
#[tokio::test]
async fn auth_sso_provider_without_a_secret_reads_like_an_unknown_one() {
    let idp = mock_idp().await;
    let k = kernel_with(Some(idp.presets())).await;
    let app = &k.app;
    add_provider(app, &idp, "half", "oidc", &[]).await;
    let secret = "/api/v1/secrets/ecosystem/00000000000000000000000000/SSO_HALF_SECRET";
    let (status, _, _) = send(app, "DELETE", secret, None, None).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    for route in ["login", "callback?code=good-code&state=whatever"] {
        let answer = |id: &str| {
            let path = format!("/api/v1/auth/sso/{id}/{route}");
            async move {
                let (status, headers, body) = send(app, "GET", &path, Some(""), None).await;
                let location = headers.get(header::LOCATION).cloned();
                (status, location, body)
            }
        };
        let half = answer("half").await;
        assert_eq!(half, answer("nope").await, "{route}");
        if route == "login" {
            assert_eq!(half.0, StatusCode::NOT_FOUND);
        }
    }
}

/// The issuer is saved as entered, trimmed of spaces only: with and
/// without a trailing slash; one without signs in as before.
#[tokio::test]
async fn auth_sso_issuer_is_kept_as_entered_with_or_without_a_slash() {
    let idp = mock_idp().await;
    let k = kernel_with(Some(idp.presets())).await;
    let app = &k.app;
    add_provider(app, &idp, "plain", "oidc", &[]).await;
    let body = json!({ "kind": "oidc", "name": "Auth0", "client_id": CLIENT_ID,
        "issuer_url": "  https://acme.auth0.com/  ", "allowed_domains": [],
        "client_secret": "client-secret" });
    let (status, _, view) = send(app, "PUT", "/api/v1/settings/sso/auth0", None, Some(body)).await;
    assert_eq!(status, StatusCode::OK, "{view}");
    let (_, _, view) = send(app, "GET", "/api/v1/settings/sso", None, None).await;
    let issuer = |id: &str| {
        view["providers"]
            .as_array()
            .unwrap()
            .iter()
            .find(|p| p["id"] == id)
            .map(|p| p["issuer_url"].clone())
    };
    assert_eq!(issuer("auth0"), Some(json!("https://acme.auth0.com/")));
    assert_eq!(issuer("plain"), Some(json!(idp.issuer)));
    let path = "/api/v1/auth/sso/plain/login";
    let (status, headers, body) = send(app, "GET", path, Some(""), None).await;
    assert_eq!(status, StatusCode::SEE_OTHER, "{body}");
    assert!(location(&headers).starts_with(&format!("{}/authorize", idp.issuer)));
}

/// `PUT /settings/security` refuses a bad body whole (an unknown field next
/// to a good one, a wrong type), keeps a rule given as `null`, and stays
/// `manage_settings` only.
#[tokio::test]
async fn auth_security_settings_refuse_bad_bodies_whole() {
    let k = kernel().await;
    let app = &k.app;
    let path = "/api/v1/settings/security";
    let put = |body: Value| send(app, "PUT", path, None, Some(body));
    let (status, _, _) = put(json!({ "require_two_step": true })).await;
    assert_eq!(status, StatusCode::OK);
    let saved = json!({ "require_two_step": true, "two_step_for_password_change": false });
    for bad in [
        json!({ "two_step_for_password_change": true, "require_2fa": false }),
        json!({ "two_step_for_password_change": "yes" }),
    ] {
        let (status, _, _) = put(bad.clone()).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{bad}");
        assert_eq!(send(app, "GET", path, None, None).await.2, saved, "{bad}");
    }
    let (status, _, kept) = put(json!({ "require_two_step": null })).await;
    assert_eq!((status, kept), (StatusCode::OK, saved.clone()));

    person(app, "bob", Role::Member, false).await;
    let bob = password_session(app, "bob@corp.io", &password_of("bob"))
        .await
        .unwrap();
    let off = Some(json!({ "require_two_step": false }));
    let (status, _, _) = send(app, "PUT", path, Some(&bob), off).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(send(app, "GET", path, None, None).await.2, saved);
}
