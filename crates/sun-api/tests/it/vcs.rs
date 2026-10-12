//! Git connections through the real kernel against loopback GitHub and
//! GitLab mocks: the signed connect state, the App and OAuth flows and
//! their landings, the workspace's repositories (with automatic GitLab
//! hooks), webhook verification, and tokens never showing in a response or
//! the audit log.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use http_body_util::BodyExt;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use sun_core::model::Role;
use sun_core::{ModCall, ModLink, Result};
use sun_mod_sdk::http::mock;
use sun_store::{SecretRepo, Store};

use crate::common::{self, SECRET, TOKEN};
use crate::route_matrix::{GITLAB_TOKEN, connect_gitlab, oauth_grant};
use crate::workspaces::{member_token, member_token_in};

const MINTED: &str = "ghs_minted_installation_token_1";
const APP_WEBHOOK_SECRET: &str = "app-webhook-secret-1";

/// Records the calls; answers `{}`.
#[derive(Default)]
struct Mods(Mutex<Vec<ModCall>>);

#[async_trait]
impl ModLink for Mods {
    async fn call(&self, call: ModCall) -> Result<Value> {
        self.0.lock().unwrap().push(call);
        Ok(json!({}))
    }
}

struct Reply {
    status: StatusCode,
    location: String,
    cookie: Option<String>,
    body: Value,
    text: String,
}

async fn send(
    app: &Router,
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
    body: Option<Value>,
) -> Reply {
    let mut request = Request::builder()
        .method(method)
        .uri(path)
        .header(header::CONTENT_TYPE, "application/json");
    for (name, value) in headers {
        request = request.header(*name, *value);
    }
    let request = request
        .body(body.map_or(Body::empty(), |b| Body::from(b.to_string())))
        .unwrap();
    let response = common::respond(app, request).await;
    let get = |name| {
        response
            .headers()
            .get(name)
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned)
    };
    let (location, cookie) = (
        get(header::LOCATION).unwrap_or_default(),
        get(header::SET_COOKIE),
    );
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    Reply {
        status,
        location,
        cookie,
        body: serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        text: String::from_utf8_lossy(&bytes).into_owned(),
    }
}

fn bearer() -> String {
    format!("Bearer {TOKEN}")
}

async fn api(app: &Router, method: &str, path: &str, body: Option<Value>) -> Reply {
    send(app, method, path, &[("authorization", &bearer())], body).await
}

async fn set_ecosystem_secret(store: &Store, name: &str, value: &str) {
    let mut tx = store.begin().await.unwrap();
    tx.set_secret(&sun_engine::ecosystem_scope().unwrap(), name, value)
        .await
        .unwrap();
    tx.commit().await.unwrap();
}

/// `state` signed the way the kernel signs it (the test kernel's host secret).
fn signed_state(state: &Value) -> String {
    let key = Sha256::new()
        .chain_update(b"sun-vcs-state\0")
        .chain_update(SECRET.as_bytes())
        .finalize();
    let payload = URL_SAFE_NO_PAD.encode(state.to_string());
    // HMAC-SHA256 by hand (RFC 2104), as the kernel computes it.
    let mut block = [0u8; 64];
    block[..key.len()].copy_from_slice(&key);
    let pad = |byte: u8| block.map(|b| b ^ byte);
    let inner = Sha256::new()
        .chain_update(pad(0x36))
        .chain_update(payload.as_bytes())
        .finalize();
    let mac = Sha256::new()
        .chain_update(pad(0x5c))
        .chain_update(inner)
        .finalize();
    let mac: String = mac.iter().map(|b| format!("{b:02x}")).collect();
    format!("{payload}.{mac}")
}

fn query_param(url: &str, name: &str) -> String {
    let start = url.find(&format!("{name}=")).unwrap() + name.len() + 1;
    let raw = url[start..].split('&').next().unwrap();
    // Percent-decode (the state is base64url + `.` + hex: only `.` may be encoded).
    raw.replace("%2E", ".")
}

/// Waits up to 10 s for `done` (the outbox delivers in the background).
async fn eventually(what: &str, done: impl Fn() -> bool) {
    for _ in 0..200 {
        if done() {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    panic!("{what}: not within 10 s");
}

/// A fake GitHub API: the App JWT reads installation 42 and mints
/// [`MINTED`]; the installation token reads its repos.
fn github_mock() -> String {
    let (url, _) = mock("", |r| {
        let auth = r.header("authorization").unwrap_or_default().to_owned();
        let jwt = auth.starts_with("Bearer ey") && auth.matches('.').count() == 2;
        let installation = auth == format!("Bearer {MINTED}");
        let web = json!({ "full_name": "acme/web", "clone_url": "https://github.com/acme/web.git",
                          "html_url": "https://github.com/acme/web", "default_branch": "trunk",
                          "description": "the site", "private": true });
        match (r.method.as_str(), r.target.as_str()) {
            ("GET", "/app/installations/42") if jwt => {
                (200, json!({ "id": 42, "account": { "login": "acme" } }))
            }
            ("POST", "/app/installations/42/access_tokens") if jwt => {
                let expires = (chrono::Utc::now() + chrono::Duration::hours(1)).to_rfc3339();
                (201, json!({ "token": MINTED, "expires_at": expires }))
            }
            ("GET", t) if t.starts_with("/installation/repositories") && installation => {
                (200, json!({ "total_count": 1, "repositories": [web] }))
            }
            ("GET", "/repos/acme/web") if installation => (200, web),
            ("GET", _) if installation => (404, json!({ "message": "Not Found" })),
            _ => (401, json!({ "message": "Bad credentials" })),
        }
    })
    .unwrap();
    url
}

#[tokio::test(flavor = "multi_thread")]
async fn vcs_list_says_which_git_apps_are_set_up_and_never_their_values() {
    let k = common::kernel().await;
    let app = &k.app;
    let main = sun_core::WorkspaceId::MAIN.to_string();
    let list = format!("/api/v1/workspaces/{main}/vcs");
    let none = json!({ "github": false, "gitlab_oauth": false });

    let reply = api(app, "GET", &list, None).await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text);
    assert_eq!(
        reply.body,
        json!({ "connections": [], "apps": none, "can_manage": true })
    );

    // Part of an app is not the app: GitHub needs all four, GitLab both.
    set_ecosystem_secret(&k.store, "GITHUB_APP_ID", "123456").await;
    set_ecosystem_secret(&k.store, "GITLAB_OAUTH_CLIENT_ID", "client-id").await;
    assert_eq!(api(app, "GET", &list, None).await.body["apps"], none);

    for (name, value) in [
        ("GITHUB_APP_SLUG", "sun-test"),
        (
            "GITHUB_APP_PRIVATE_KEY",
            "-----BEGIN PRIVATE KEY-----sealed",
        ),
        ("GITHUB_APP_WEBHOOK_SECRET", APP_WEBHOOK_SECRET),
        ("GITLAB_OAUTH_CLIENT_SECRET", "client-secret-1"),
    ] {
        set_ecosystem_secret(&k.store, name, value).await;
    }
    let all = json!({ "github": true, "gitlab_oauth": true });
    assert_eq!(api(app, "GET", &list, None).await.body["apps"], all);

    // Anyone who sees the workspace reads the flags (no manage_secrets), and no value.
    let viewer = format!(
        "Bearer {}",
        member_token(&k.store, "vic", Role::Viewer).await
    );
    let reply = send(app, "GET", &list, &[("authorization", &viewer)], None).await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text);
    assert_eq!(reply.body["apps"], all);
    assert_eq!(reply.body["can_manage"], false, "a viewer can't connect");
    for value in [
        "sealed",
        APP_WEBHOOK_SECRET,
        "client-secret-1",
        "client-id",
        "123456",
    ] {
        assert!(!reply.text.contains(value), "{value} in {}", reply.text);
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn vcs_github_app_connects_with_a_signed_state_and_adds_repos() {
    let k = common::kernel().await;
    let app = &k.app;
    let main = sun_core::WorkspaceId::MAIN.to_string();
    let connect = format!("/api/v1/workspaces/{main}/vcs/github/connect");

    // Not set up yet: nothing to open (the UI shows the operator copy).
    let reply = api(app, "GET", &connect, None).await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text);
    assert_eq!(reply.body, json!({ "url": null, "configured": false }));
    assert!(reply.cookie.is_none());

    let pem = include_str!("../../../sun-engine/src/vcs/test_app_key.pem");
    for (name, value) in [
        ("GITHUB_APP_ID", "123456"),
        ("GITHUB_APP_SLUG", "sun-test"),
        ("GITHUB_APP_PRIVATE_KEY", pem),
        ("GITHUB_APP_WEBHOOK_SECRET", APP_WEBHOOK_SECRET),
        ("GITHUB_APP_API_URL", &github_mock()),
    ] {
        set_ecosystem_secret(&k.store, name, value).await;
    }
    let reply = api(app, "GET", &connect, None).await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text);
    assert_eq!(reply.body["configured"], true);
    let url = reply.body["url"].as_str().unwrap().to_owned();
    assert!(
        url.starts_with("https://github.com/apps/sun-test/installations/new?state="),
        "{url}"
    );
    // Only the two landings the UI knows.
    let bogus = format!("{connect}?return_to=elsewhere");
    assert_eq!(
        api(app, "GET", &bogus, None).await.status,
        StatusCode::UNPROCESSABLE_ENTITY
    );
    let cookie = reply.cookie.unwrap();
    assert!(
        cookie.contains("HttpOnly") && cookie.contains("SameSite=Lax"),
        "{cookie}"
    );
    assert!(cookie.starts_with("sun_vcs_state_github="), "{cookie}");
    let browser = cookie.split(';').next().unwrap().to_owned();
    // A GitLab connect started meanwhile in another tab has its own cookie.
    set_ecosystem_secret(&k.store, "GITLAB_OAUTH_CLIENT_ID", "client-id").await;
    set_ecosystem_secret(&k.store, "GITLAB_OAUTH_CLIENT_SECRET", "client-secret-1").await;
    let other_tab = api(
        app,
        "GET",
        &format!("/api/v1/workspaces/{main}/vcs/gitlab/connect"),
        None,
    )
    .await;
    assert_eq!(other_tab.status, StatusCode::OK, "{}", other_tab.text);
    let gitlab_cookie = other_tab.cookie.unwrap();
    assert!(
        gitlab_cookie.starts_with("sun_vcs_state_gitlab="),
        "{gitlab_cookie}"
    );
    let both = format!("{browser}; {}", gitlab_cookie.split(';').next().unwrap());
    let state = query_param(&url, "state");
    let setup = |state: &str| format!("/api/v1/vcs/github/setup?installation_id=42&state={state}");

    // Refused: no cookie (another browser), a forged state, an expired one,
    // and a GitLab state on the GitHub callback.
    let invalid = "/#/settings/code?github_error=invalid_state";
    let reply = send(app, "GET", "/api/v1/vcs/github/setup", &[], None).await;
    assert_eq!(
        reply.location,
        "/#/settings/code?github_error=missing_params"
    );
    let reply = send(app, "GET", &setup(&state), &[], None).await;
    assert_eq!(
        (reply.status, reply.location.as_str()),
        (StatusCode::SEE_OTHER, invalid)
    );
    let (payload, mac) = state.split_once('.').unwrap();
    let mut forged: Value =
        serde_json::from_slice(&URL_SAFE_NO_PAD.decode(payload).unwrap()).unwrap();
    let nonce = forged["nonce"].as_str().unwrap().to_owned();
    forged["workspace"] = json!(sun_core::WorkspaceId::generate());
    let forged = format!("{}.{mac}", URL_SAFE_NO_PAD.encode(forged.to_string()));
    let mut expired: Value =
        serde_json::from_slice(&URL_SAFE_NO_PAD.decode(payload).unwrap()).unwrap();
    expired["expires"] = json!(chrono::Utc::now().timestamp() - 1);
    let mut gitlab = expired.clone();
    gitlab["expires"] = json!(chrono::Utc::now().timestamp() + 60);
    gitlab["provider"] = json!("gitlab");
    for bad in [
        forged,
        signed_state(&expired),
        signed_state(&gitlab),
        "x.y".into(),
    ] {
        let reply = send(app, "GET", &setup(&bad), &[("cookie", &browser)], None).await;
        assert_eq!(reply.location, invalid, "{bad}");
    }
    // A cookie with another nonce: refused too.
    let other = format!("sun_vcs_state_github={}x", &nonce[..nonce.len() - 1]);
    let reply = send(app, "GET", &setup(&state), &[("cookie", &other)], None).await;
    assert_eq!(reply.location, invalid);
    // An installation GitHub doesn't know, or not a number: named as such.
    let unknown = format!("/api/v1/vcs/github/setup?installation_id=7&state={state}");
    let reply = send(app, "GET", &unknown, &[("cookie", &browser)], None).await;
    assert_eq!(
        reply.location,
        format!("/#/settings/code?workspace={main}&github_error=bad_installation_id")
    );
    let text = format!("/api/v1/vcs/github/setup?installation_id=abc&state={state}");
    let reply = send(app, "GET", &text, &[("cookie", &browser)], None).await;
    assert!(
        reply.location.ends_with("github_error=bad_installation_id"),
        "{}",
        reply.location
    );
    let none = format!("/api/v1/vcs/github/setup?state={state}");
    let reply = send(app, "GET", &none, &[("cookie", &browser)], None).await;
    assert!(
        reply.location.ends_with("github_error=missing_params"),
        "{}",
        reply.location
    );

    let reply = send(app, "GET", &setup(&state), &[("cookie", &both)], None).await;
    assert_eq!(reply.status, StatusCode::SEE_OTHER);
    assert_eq!(
        reply.location,
        format!("/#/settings/code?workspace={main}&github_connected=1")
    );
    assert!(
        reply.cookie.unwrap().contains("Max-Age=0"),
        "the state cookie is cleared"
    );
    // Started from the repositories card: lands with the picker open.
    let reply = api(
        app,
        "GET",
        &format!("{connect}?return_to=repositories"),
        None,
    )
    .await;
    let state = query_param(reply.body["url"].as_str().unwrap(), "state");
    let browser = reply.cookie.unwrap().split(';').next().unwrap().to_owned();
    let reply = send(app, "GET", &setup(&state), &[("cookie", &browser)], None).await;
    assert_eq!(
        reply.location,
        format!("/#/settings/code?workspace={main}&github_connected=1&return_to=repositories")
    );

    let reply = api(app, "GET", &format!("/api/v1/workspaces/{main}/vcs"), None).await;
    let connection = reply.body["connections"][0].clone();
    assert_eq!(connection["provider"], "github");
    assert_eq!(connection["account_login"], "acme");
    assert_eq!(connection["installation_id"], 42);
    assert!(
        connection["connected_by"]
            .as_str()
            .unwrap()
            .starts_with("user:")
    );
    let id = connection["id"].as_str().unwrap().to_owned();

    // The picker, with the minted installation token.
    let picker = format!("/api/v1/workspaces/{main}/vcs/{id}/repos?q=WEB");
    let reply = api(app, "GET", &picker, None).await;
    assert_eq!(reply.body[0]["name"], "acme/web", "{}", reply.text);
    assert!(!reply.text.contains(MINTED));

    // A repo on a connected host: its default branch comes from GitHub.
    let repos = format!("/api/v1/workspaces/{main}/repos");
    let add = json!({ "url": "https://github.com/acme/web.git", "description": "Customer site" });
    let reply = api(app, "POST", &repos, Some(add)).await;
    assert_eq!(reply.status, StatusCode::CREATED, "{}", reply.text);
    let repo = reply.body["repo"].clone();
    assert_eq!(repo["default_branch"], "trunk");
    assert_eq!(repo["workspace_id"], main.as_str());
    assert_eq!(repo["description"], "Customer site");
    assert_eq!(reply.body["manual_hook"], Value::Null);
    // A repo the App can't see: 422. On a host with no connection: added
    // as is, on `main`. The same URL twice, or one with credentials: 422.
    let hidden = json!({ "url": "https://github.com/acme/secret" });
    assert_eq!(
        api(app, "POST", &repos, Some(hidden)).await.status,
        StatusCode::UNPROCESSABLE_ENTITY
    );
    let elsewhere = json!({ "url": "https://gitlab.com/acme/web.git" });
    let reply = api(app, "POST", &repos, Some(elsewhere.clone())).await;
    assert_eq!(reply.status, StatusCode::CREATED, "{}", reply.text);
    assert_eq!(reply.body["repo"]["default_branch"], "main");
    let reply = api(app, "POST", &repos, Some(elsewhere)).await;
    assert_eq!(reply.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(
        reply.text.contains("This repository is already added."),
        "{}",
        reply.text
    );
    let with_token = json!({ "url": "https://x:ghp_pasted_token@github.com/acme/web.git" });
    let refused = api(app, "POST", &repos, Some(with_token)).await;
    assert_eq!(refused.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(!refused.text.contains("ghp_pasted"), "{}", refused.text);

    // Edit, then the context of every run lists it.
    let repo_id = repo["id"].as_str().unwrap();
    let edit = json!({ "description": "The customer site", "default_branch": "main" });
    let reply = api(
        app,
        "PATCH",
        &format!("/api/v1/repos/{repo_id}"),
        Some(edit),
    )
    .await;
    assert_eq!(
        reply.body["description"], "The customer site",
        "{}",
        reply.text
    );
    let bad = json!({ "default_branch": "../x" });
    let reply = api(app, "PATCH", &format!("/api/v1/repos/{repo_id}"), Some(bad)).await;
    assert_eq!(reply.status, StatusCode::UNPROCESSABLE_ENTITY);

    // Another workspace's connection answers 404: through the other
    // workspace's path, and to a member who doesn't see Main.
    let reply = api(
        app,
        "POST",
        "/api/v1/workspaces",
        Some(json!({ "name": "X" })),
    )
    .await;
    let x = reply.body["id"].as_str().unwrap().to_owned();
    let across = format!("/api/v1/workspaces/{x}/vcs/{id}/repos");
    assert_eq!(
        api(app, "GET", &across, None).await.status,
        StatusCode::NOT_FOUND
    );
    let x_id: sun_core::WorkspaceId = x.parse().unwrap();
    let bob = member_token_in(&k.store, "bob", &[(x_id, Role::Member)]).await;
    let auth = format!("Bearer {bob}");
    let delete = format!("/api/v1/vcs/connections/{id}");
    for (method, path) in [
        ("GET", format!("/api/v1/workspaces/{main}/vcs")),
        ("DELETE", delete.clone()),
        (
            "POST",
            format!("/api/v1/vcs/connections/{id}/rotate-webhook"),
        ),
        ("GET", across.clone()),
    ] {
        let reply = send(app, method, &path, &[("authorization", &auth)], None).await;
        assert_eq!(reply.status, StatusCode::NOT_FOUND, "{method} {path}");
    }

    // Never a token: not in any answer above, not in the audit log.
    let audit = api(app, "GET", "/api/v1/audit", None).await;
    assert!(
        audit.text.contains("connected GitHub acme"),
        "{}",
        audit.text
    );
    assert!(!audit.text.contains(MINTED) && !audit.text.contains("BEGIN"));
    let secrets = api(app, "GET", "/api/v1/secrets", None).await;
    assert!(!secrets.text.contains("\"workspace\""), "{}", secrets.text);

    // Disconnect: the repos stay.
    assert_eq!(
        api(app, "DELETE", &delete, None).await.status,
        StatusCode::NO_CONTENT
    );
    let reply = api(app, "GET", &format!("/api/v1/workspaces/{main}/vcs"), None).await;
    assert_eq!(reply.body["connections"], json!([]));
    let reply = api(app, "GET", &repos, None).await;
    assert_eq!(reply.body.as_array().unwrap().len(), 2, "{}", reply.text);
    assert_eq!(reply.body[0]["id"], repo_id);
}

#[tokio::test(flavor = "multi_thread")]
async fn vcs_github_app_webhooks_are_verified_with_the_app_secret() {
    let mods = Arc::new(Mods::default());
    let k = common::kernel_with_mods(mods.clone()).await;
    set_ecosystem_secret(&k.store, "GITHUB_APP_WEBHOOK_SECRET", APP_WEBHOOK_SECRET).await;
    let body = json!({ "action": "labeled", "issue": { "number": 1 } }).to_string();
    let sign = |secret: &str| {
        let mut block = [0u8; 64];
        block[..secret.len()].copy_from_slice(secret.as_bytes());
        let pad = |byte: u8| block.map(|b| b ^ byte);
        let inner = Sha256::new()
            .chain_update(pad(0x36))
            .chain_update(&body)
            .finalize();
        let mac = Sha256::new()
            .chain_update(pad(0x5c))
            .chain_update(inner)
            .finalize();
        format!(
            "sha256={}",
            mac.iter().map(|b| format!("{b:02x}")).collect::<String>()
        )
    };
    let deliver = |signature: String| {
        let body = body.clone();
        let app = k.app.clone();
        async move {
            let request = Request::builder()
                .method("POST")
                .uri("/api/v1/webhooks/github")
                .header("x-github-event", "issues")
                .header("x-hub-signature-256", signature)
                .body(Body::from(body))
                .unwrap();
            common::respond(&app, request).await.status()
        }
    };
    assert_eq!(
        deliver(sign("wrong-secret-1")).await,
        StatusCode::UNAUTHORIZED
    );
    assert!(mods.0.lock().unwrap().is_empty());
    assert_eq!(deliver(sign(APP_WEBHOOK_SECRET)).await, StatusCode::OK);
    let calls = mods.0.lock().unwrap();
    assert_eq!(calls[0].mod_name, "github");
    assert_eq!(calls[0].params["event"], "issues");
}

/// A fake GitLab: any OAuth code grants [`GITLAB_TOKEN`], `GET /user` for
/// it, projects `acme/app` (hooks allowed) and `acme/other` (the user isn't
/// a maintainer: 403).
fn gitlab_mock(hooks: Arc<Mutex<Vec<String>>>) -> String {
    let mut next_hook = 76;
    let (url, _) = mock("", move |r| {
        if r.method == "POST" && r.target == "/oauth/token" {
            return (200, oauth_grant());
        }
        if r.header("authorization") != Some(&format!("Bearer {GITLAB_TOKEN}")) {
            return (401, json!({ "message": "401 Unauthorized" }));
        }
        let project = |name: &str| {
            json!({ "id": 9, "path_with_namespace": format!("acme/{name}"),
            "http_url_to_repo": format!("https://gitlab.test/acme/{name}.git"),
            "default_branch": "main", "visibility": "private" })
        };
        let mut seen = hooks.lock().unwrap();
        seen.push(format!("{} {} {}", r.method, r.target, r.body));
        match (r.method.as_str(), r.target.as_str()) {
            ("GET", "/api/v4/user") => (200, json!({ "username": "ann" })),
            ("GET", "/api/v4/projects/acme%2Fapp") => (200, project("app")),
            ("GET", "/api/v4/projects/acme%2Fother") => (200, project("other")),
            ("POST", "/api/v4/projects/acme%2Fapp/hooks") => {
                next_hook += 1;
                (201, json!({ "id": next_hook }))
            }
            ("POST", "/api/v4/projects/acme%2Fother/hooks") => {
                (403, json!({ "message": "403 Forbidden" }))
            }
            ("PUT", t) if t.starts_with("/api/v4/projects/acme%2Fapp/hooks/") => {
                (200, json!({ "id": 77 }))
            }
            ("DELETE", t) if t.starts_with("/api/v4/projects/acme%2Fapp/hooks/") => {
                (204, json!({}))
            }
            _ => (404, json!({ "message": "404 Not Found" })),
        }
    })
    .unwrap();
    url
}

#[tokio::test(flavor = "multi_thread")]
async fn vcs_gitlab_oauth_makes_hooks_and_verifies_their_deliveries() {
    let mods = Arc::new(Mods::default());
    let k = common::kernel_with_mods(mods.clone()).await;
    let app = &k.app;
    let main = sun_core::WorkspaceId::MAIN.to_string();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let gitlab = gitlab_mock(seen.clone());
    let connect = format!("/api/v1/workspaces/{main}/vcs/gitlab/connect");

    // Not set up yet: nothing to open.
    let reply = api(app, "GET", &connect, None).await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text);
    assert_eq!(reply.body, json!({ "url": null, "configured": false }));
    set_ecosystem_secret(&k.store, "GITLAB_OAUTH_URL", &gitlab).await;
    set_ecosystem_secret(&k.store, "GITLAB_OAUTH_CLIENT_ID", "client-id").await;
    set_ecosystem_secret(&k.store, "GITLAB_OAUTH_CLIENT_SECRET", "client-secret-1").await;
    // Back from GitLab without a state, or after the person denied: a
    // reason, nothing stored.
    let reply = send(app, "GET", "/api/v1/vcs/gitlab/callback", &[], None).await;
    assert_eq!(
        reply.location,
        "/#/settings/code?gitlab_error=missing_params"
    );
    let reply = api(app, "GET", &connect, None).await;
    assert_eq!(reply.body["configured"], true, "{}", reply.text);
    let state = query_param(reply.body["url"].as_str().unwrap(), "state");
    let browser = reply.cookie.unwrap().split(';').next().unwrap().to_owned();
    let denied = format!("/api/v1/vcs/gitlab/callback?error=access_denied&state={state}");
    let reply = send(app, "GET", &denied, &[("cookie", &browser)], None).await;
    assert_eq!(
        reply.location,
        format!("/#/settings/code?workspace={main}&gitlab_error=denied")
    );
    let reply = api(app, "GET", &format!("/api/v1/workspaces/{main}/vcs"), None).await;
    assert_eq!(reply.body["connections"], json!([]));

    let connection = connect_gitlab(&k, &main, &gitlab).await;
    assert_eq!(connection["account_login"], "ann");
    let text = connection.to_string();
    assert!(
        !text.contains(GITLAB_TOKEN) && !text.contains("secret"),
        "{text}"
    );
    let id = connection["id"].as_str().unwrap().to_owned();

    // Adding a repo makes its hook, signed with the connection's secret.
    let repos = format!("/api/v1/workspaces/{main}/repos");
    let app_url = format!("{gitlab}/acme/app.git");
    let reply = api(app, "POST", &repos, Some(json!({ "url": app_url }))).await;
    assert_eq!(reply.status, StatusCode::CREATED, "{}", reply.text);
    assert_eq!(reply.body["manual_hook"], Value::Null);
    let repo_id = reply.body["repo"]["id"].as_str().unwrap().to_owned();
    let hook: Value = {
        let seen = seen.lock().unwrap();
        let post = seen
            .iter()
            .find(|s| s.starts_with("POST /api/v4/projects/acme%2Fapp/hooks"))
            .unwrap();
        serde_json::from_str(post.splitn(3, ' ').nth(2).unwrap()).unwrap()
    };
    assert_eq!(
        hook["url"],
        format!("http://localhost/api/v1/webhooks/vcs/{id}")
    );
    assert_eq!(hook["merge_requests_events"], true);
    let secret = hook["token"].as_str().unwrap().to_owned();

    // Not a maintainer of `other`: added anyway, with the URL and secret to paste.
    let other = format!("{gitlab}/acme/other.git");
    let reply = api(app, "POST", &repos, Some(json!({ "url": other }))).await;
    assert_eq!(reply.status, StatusCode::CREATED, "{}", reply.text);
    assert_eq!(reply.body["manual_hook"]["secret"], secret.as_str());

    // Deliveries: a bad or missing token, or an unknown connection, is 401.
    let hook_url = format!("/api/v1/webhooks/vcs/{id}");
    let note = json!({ "object_kind": "note" });
    let deliver = |path: String, token: Option<String>| {
        let app = k.app.clone();
        let body = note.to_string();
        async move {
            let mut request = Request::builder()
                .method("POST")
                .uri(path)
                .header("x-gitlab-event", "Note Hook");
            if let Some(token) = token {
                request = request.header("x-gitlab-token", token);
            }
            common::respond(&app, request.body(Body::from(body)).unwrap())
                .await
                .status()
        }
    };
    assert_eq!(
        deliver(hook_url.clone(), Some("nope-nope".into())).await,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        deliver(hook_url.clone(), None).await,
        StatusCode::UNAUTHORIZED
    );
    let unknown = format!(
        "/api/v1/webhooks/vcs/{}",
        sun_core::VcsConnectionId::generate()
    );
    assert_eq!(
        deliver(unknown, Some(secret.clone())).await,
        StatusCode::UNAUTHORIZED
    );
    assert!(mods.0.lock().unwrap().is_empty());
    assert_eq!(
        deliver(hook_url.clone(), Some(secret.clone())).await,
        StatusCode::OK
    );
    {
        let calls = mods.0.lock().unwrap();
        assert_eq!(
            (calls[0].mod_name.as_str(), calls[0].method.as_str()),
            ("gitlab", "webhook")
        );
        assert_eq!(calls[0].params["event"], "Note Hook");
    }

    // Rotating updates the hook; the old secret stops working.
    let rotate = format!("/api/v1/vcs/connections/{id}/rotate-webhook");
    let reply = api(app, "POST", &rotate, None).await;
    assert_eq!(reply.body["updated"], 1, "{}", reply.text);
    assert_eq!(reply.body["manual_hook"], Value::Null);
    assert_eq!(
        deliver(hook_url, Some(secret.clone())).await,
        StatusCode::UNAUTHORIZED
    );

    // Removing the repo deletes its hook through the outbox; the unused row goes.
    let remove = format!("/api/v1/workspaces/{main}/repos/{repo_id}");
    assert_eq!(
        api(app, "DELETE", &remove, None).await.status,
        StatusCode::NO_CONTENT
    );
    let deleted = |hook: u32| {
        let path = format!("DELETE /api/v4/projects/acme%2Fapp/hooks/{hook}");
        seen.lock().unwrap().iter().any(|s| s.starts_with(&path))
    };
    eventually("hook 77 deleted", || deleted(77)).await;
    let reply = api(app, "GET", &repos, None).await;
    assert_eq!(reply.body.as_array().unwrap().len(), 1, "{}", reply.text);

    // Disconnecting deletes the remaining hooks (outbox), then the secrets.
    let app_url = format!("{gitlab}/acme/app.git");
    let reply = api(app, "POST", &repos, Some(json!({ "url": app_url }))).await;
    assert_eq!(reply.status, StatusCode::CREATED, "{}", reply.text);
    let disconnect = format!("/api/v1/vcs/connections/{id}");
    assert_eq!(
        api(app, "DELETE", &disconnect, None).await.status,
        StatusCode::NO_CONTENT
    );
    eventually("hook 78 deleted", || deleted(78)).await;
    let scope = sun_core::model::SecretScope::Workspace(sun_core::WorkspaceId::MAIN);
    let mut gone = false;
    for _ in 0..200 {
        gone = k
            .store
            .read()
            .await
            .unwrap()
            .list_secrets(Some(&scope))
            .await
            .unwrap()
            .is_empty();
        if gone {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    assert!(gone, "the token and secrets go after the last hook");

    // The token and the secret never reach the audit log.
    let audit = api(app, "GET", "/api/v1/audit", None).await;
    assert!(
        audit.text.contains("connected GitLab ann"),
        "{}",
        audit.text
    );
    assert!(!audit.text.contains(GITLAB_TOKEN) && !audit.text.contains(&secret));
}

/// GitLab answers 401 on `/oauth/token` (the operator's app secrets don't
/// match): the landing says `refused`, not `failed`. The secrets reach
/// GitLab trimmed, so a pasted newline isn't the cause.
#[tokio::test(flavor = "multi_thread")]
async fn vcs_gitlab_callback_says_refused_when_the_token_exchange_fails() {
    let k = common::kernel().await;
    let app = &k.app;
    let main = sun_core::WorkspaceId::MAIN.to_string();
    let forms = Arc::new(Mutex::new(Vec::new()));
    let seen = forms.clone();
    let (gitlab, _) = mock("", move |r| {
        seen.lock().unwrap().push(r.text.clone());
        (401, json!({ "error": "invalid_client" }))
    })
    .unwrap();
    set_ecosystem_secret(&k.store, "GITLAB_OAUTH_URL", &gitlab).await;
    set_ecosystem_secret(&k.store, "GITLAB_OAUTH_CLIENT_ID", "client-id").await;
    set_ecosystem_secret(&k.store, "GITLAB_OAUTH_CLIENT_SECRET", " client-secret-1\n").await;
    let connect = format!("/api/v1/workspaces/{main}/vcs/gitlab/connect");
    let reply = api(app, "GET", &connect, None).await;
    let state = query_param(reply.body["url"].as_str().unwrap(), "state");
    let browser = reply.cookie.unwrap().split(';').next().unwrap().to_owned();
    let callback = format!("/api/v1/vcs/gitlab/callback?code=dummy-code&state={state}");
    let reply = send(app, "GET", &callback, &[("cookie", &browser)], None).await;
    assert_eq!(
        reply.location,
        format!("/#/settings/code?workspace={main}&gitlab_error=refused")
    );
    let forms = forms.lock().unwrap().clone();
    assert_eq!(forms.len(), 1, "{forms:?}");
    assert!(
        forms[0].ends_with("client_secret=client-secret-1"),
        "{}",
        forms[0]
    );
    let reply = api(app, "GET", &format!("/api/v1/workspaces/{main}/vcs"), None).await;
    assert_eq!(reply.body["connections"], json!([]));
}

#[tokio::test(flavor = "multi_thread")]
async fn repos_take_every_clone_form_but_no_file_paths_or_credentials_and_stay_per_workspace() {
    let k = common::kernel().await;
    let app = &k.app;
    let main = sun_core::WorkspaceId::MAIN.to_string();
    let repos = format!("/api/v1/workspaces/{main}/repos");
    let add = |url: &str| api(app, "POST", &repos, Some(json!({ "url": url })));

    for bad in [
        "file:///tmp/repo",
        "/tmp/repo",
        "./repo",
        "repo",
        "https://",
        "https://user:ghp_dummy_token@github.com/acme/web.git",
        "ftp://github.com/acme/web.git",
        "-https://github.com/acme/web.git",
        "",
    ] {
        let reply = add(bad).await;
        assert_eq!(reply.status, StatusCode::UNPROCESSABLE_ENTITY, "{bad:?}");
        assert!(!reply.text.contains("ghp_dummy"), "{}", reply.text);
    }
    assert_eq!(
        api(app, "POST", &repos, Some(json!({}))).await.status,
        StatusCode::UNPROCESSABLE_ENTITY
    );
    assert_eq!(api(app, "GET", &repos, None).await.body, json!([]));

    // http (loopback only), https, ssh://, git:// and the scp form.
    let good = [
        "https://github.com/acme/web.git",
        "ssh://git@github.com/acme/api.git",
        "git://github.com/acme/cli.git",
        "git@github.com:acme/ops.git",
        "http://localhost:3000/acme/local.git",
    ];
    for url in good {
        let reply = add(url).await;
        assert_eq!(reply.status, StatusCode::CREATED, "{url}: {}", reply.text);
        assert_eq!(reply.body["repo"]["url"], url);
        assert_eq!(reply.body["repo"]["default_branch"], "main");
    }
    // Duplicates are by exact URL: the SSH form of an added HTTPS repo is a
    // second row (the UI's Add dialog catches that case before saving).
    let reply = add("git@github.com:acme/web.git").await;
    assert_eq!(reply.status, StatusCode::CREATED, "{}", reply.text);
    let reply = add("https://github.com/acme/web.git").await;
    assert_eq!(reply.status, StatusCode::UNPROCESSABLE_ENTITY);
    let listed = api(app, "GET", &repos, None).await.body;
    assert_eq!(listed.as_array().unwrap().len(), good.len() + 1, "{listed}");

    // A second workspace starts with no repos and no connections of its own.
    let reply = api(
        app,
        "POST",
        "/api/v1/workspaces",
        Some(json!({ "name": "Ops" })),
    )
    .await;
    let ops = reply.body["id"].as_str().unwrap().to_owned();
    assert_eq!(
        api(app, "GET", &format!("/api/v1/workspaces/{ops}/repos"), None)
            .await
            .body,
        json!([])
    );
    let reply = api(app, "GET", &format!("/api/v1/workspaces/{ops}/vcs"), None).await;
    assert_eq!(reply.body["connections"], json!([]));
    // The same URL is fine there: uniqueness is per workspace.
    let reply = api(
        app,
        "POST",
        &format!("/api/v1/workspaces/{ops}/repos"),
        Some(json!({ "url": "https://github.com/acme/web.git" })),
    )
    .await;
    assert_eq!(reply.status, StatusCode::CREATED, "{}", reply.text);
    // Main's repo through Ops' path: 404, and Ops' repo through Main's.
    let main_repo = listed[0]["id"].as_str().unwrap();
    let ops_repo = reply.body["repo"]["id"].as_str().unwrap().to_owned();
    for path in [
        format!("/api/v1/workspaces/{ops}/repos/{main_repo}"),
        format!("/api/v1/workspaces/{main}/repos/{ops_repo}"),
    ] {
        assert_eq!(
            api(app, "DELETE", &path, None).await.status,
            StatusCode::NOT_FOUND,
            "{path}"
        );
    }
}
