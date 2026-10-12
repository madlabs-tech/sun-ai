//! Abuse of the workspaces refactor's routes: lying and oversized logos,
//! every illegal move, task keys and projects across workspaces, prefix
//! collisions, forged webhooks, replays, and a used connect state.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use sun_core::model::Role;
use sun_core::{Error, ModCall, ModLink, Result, StateMachine, TaskStatus};
use sun_mod_sdk::http::mock;
use sun_store::SecretRepo;

use crate::common::{self, TOKEN, kernel};
use crate::route_matrix::{
    GITLAB_TOKEN, HookSecrets, MAIN, PNG, connect_gitlab, gitlab_stub_seeing, ok, raw, world,
};
use crate::workspaces::{call, call_as, member_token};

/// Records the mod calls; answers `{}`.
#[derive(Default)]
struct Mods(Mutex<Vec<ModCall>>, AtomicBool);

impl Mods {
    /// The next call fails, as a mod that is down.
    fn fail_next(&self) {
        self.1.store(true, Ordering::SeqCst);
    }
}

#[async_trait]
impl ModLink for Mods {
    async fn call(&self, call: ModCall) -> Result<Value> {
        self.0.lock().unwrap().push(call);
        if self.1.swap(false, Ordering::SeqCst) {
            return Err(Error::Unavailable("the mod is down".into()));
        }
        Ok(json!({}))
    }
}

pub(crate) fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// HMAC-SHA256 (RFC 2104) of `body` with `key` (≤ 64 bytes).
pub(crate) fn hmac(key: &[u8], body: &[u8]) -> Vec<u8> {
    let mut block = [0u8; 64];
    block[..key.len()].copy_from_slice(key);
    let pad = |byte: u8| block.map(|b| b ^ byte);
    let inner = Sha256::new()
        .chain_update(pad(0x36))
        .chain_update(body)
        .finalize();
    Sha256::new()
        .chain_update(pad(0x5c))
        .chain_update(inner)
        .finalize()
        .to_vec()
}

pub(crate) async fn post(
    app: &Router,
    path: &str,
    headers: &[(&str, &str)],
    body: &str,
) -> StatusCode {
    let mut request = Request::builder().method("POST").uri(path);
    for (name, value) in headers {
        request = request.header(*name, *value);
    }
    let request = request.body(Body::from(body.to_owned())).unwrap();
    common::respond(app, request).await.status()
}

#[tokio::test]
async fn logos_refuse_lying_empty_and_oversized_images() {
    let k = kernel().await;
    let app = &k.app;
    let path = format!("/api/v1/workspaces/{MAIN}/logo");
    let put = |kind: &'static str, body: Vec<u8>| raw(app, TOKEN, "PUT", &path, kind, body);
    let webp = b"RIFF\x10\0\0\0WEBPVP8 ".to_vec();
    let jpeg = b"\xff\xd8\xff\xe0\0\x10JFIF".to_vec();
    let exactly_1_mib = [PNG, &vec![0; 1024 * 1024 - PNG.len()]].concat();
    for (kind, body) in [
        ("image/webp", PNG.to_vec()),
        ("image/png", webp.clone()),
        ("image/gif", b"GIF89a\x01\0\x01\0".to_vec()),
        ("image/png", Vec::new()),
        ("text/html", PNG.to_vec()),
        ("", PNG.to_vec()),
        ("image/png", b"<svg onload=alert(1)>".to_vec()),
        ("image/png", [exactly_1_mib.as_slice(), b"x"].concat()),
    ] {
        assert_eq!(
            put(kind, body).await,
            StatusCode::UNPROCESSABLE_ENTITY,
            "{kind}"
        );
    }
    // Far too big: refused before it is read, never a 5xx.
    let status = put("image/png", vec![0; 3 * 1024 * 1024]).await;
    assert!(
        matches!(
            status,
            StatusCode::PAYLOAD_TOO_LARGE | StatusCode::UNPROCESSABLE_ENTITY
        ),
        "{status}"
    );
    for (kind, body) in [
        ("image/png", exactly_1_mib),
        ("image/webp", webp),
        ("IMAGE/JPEG; charset=binary", jpeg),
    ] {
        assert_eq!(put(kind, body).await, StatusCode::OK, "{kind}");
    }
}

/// From `todo`, the moves that bring a plain task to `status`.
fn path_to(status: TaskStatus) -> Vec<TaskStatus> {
    match status {
        TaskStatus::Todo => vec![],
        TaskStatus::Done => vec![TaskStatus::NeedsYou, TaskStatus::Done],
        other => vec![other],
    }
}

#[tokio::test]
async fn every_move_of_a_plain_task_follows_the_one_rules_table() {
    let k = kernel().await;
    let app = &k.app;
    for &from in TaskStatus::ALL {
        for &to in TaskStatus::ALL {
            if from == to {
                continue;
            }
            let task = ok(
                app,
                "POST",
                "/api/v1/tasks",
                Some(json!({ "title": "t", "workspace_id": MAIN })),
            )
            .await;
            let path = format!("/api/v1/tasks/{}", task["key"].as_str().unwrap());
            for step in path_to(from) {
                ok(app, "PATCH", &path, Some(json!({ "status": step }))).await;
            }
            let (status, reply) = call(app, "PATCH", &path, Some(json!({ "status": to }))).await;
            let want = if from.can_transition_to(to) {
                StatusCode::OK
            } else {
                StatusCode::CONFLICT
            };
            assert_eq!(status, want, "{from} -> {to}: {reply}");
            if status == StatusCode::CONFLICT {
                assert!(
                    reply["error"]["message"].as_str().unwrap().ends_with('.'),
                    "a plain sentence: {reply}"
                );
            }
        }
    }
    let task = ok(
        app,
        "POST",
        "/api/v1/tasks",
        Some(json!({ "title": "t", "workspace_id": MAIN })),
    )
    .await;
    let path = format!("/api/v1/tasks/{}", task["id"].as_str().unwrap());
    for bad in [json!("Done"), json!("DONE"), json!(""), json!(4)] {
        let (status, _) = call(app, "PATCH", &path, Some(json!({ "status": bad }))).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{bad}");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn task_keys_and_projects_never_cross_workspaces() {
    let w = world().await;
    let app = &w.k.app;
    // Even an admin can't file Main's task under Ops' project.
    let path = format!("/api/v1/tasks/{}", w.main.plain);
    let (status, _) = call(
        app,
        "PATCH",
        &path,
        Some(json!({ "project_id": w.ops.project })),
    )
    .await;
    assert!(status.is_client_error(), "{status}");
    let (_, view) = call(app, "GET", &path, None).await;
    assert_eq!(view["project_id"], Value::Null);
    let cross = json!({ "title": "x", "workspace_id": MAIN, "project_id": w.ops.project });
    let (status, _) = call(app, "POST", "/api/v1/tasks", Some(cross)).await;
    assert!(status.is_client_error(), "{status}");

    // Odd keys: never a server error.
    for key in [
        "MAIN-0",
        "MAIN--1",
        "-1",
        "MAIN-",
        "MAIN-18446744073709551616",
        "MAIN-99999999999",
        "M%20AIN-1",
        "%C3%84-1",
        "main-1%00",
    ] {
        let (status, _) = call(app, "GET", &format!("/api/v1/tasks/{key}"), None).await;
        assert!(!status.is_server_error(), "{key}: {status}");
        assert_ne!(status, StatusCode::OK, "{key}");
    }

    // A new prefix renumbers: the old key stops resolving, the new one works.
    let main_task = call(app, "GET", &path, None).await.1["id"].clone();
    let old_key = w.main.plain.clone();
    let number = old_key.rsplit('-').next().unwrap();
    ok(
        app,
        "POST",
        &format!("/api/v1/workspaces/{MAIN}/prefix"),
        Some(json!({ "issue_prefix": "core" })),
    )
    .await;
    let (status, _) = call(app, "GET", &format!("/api/v1/tasks/{old_key}"), None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, view) = call(app, "GET", &format!("/api/v1/tasks/CORE-{number}"), None).await;
    assert_eq!((status, &view["id"]), (StatusCode::OK, &main_task));
    // Ops takes the freed prefix: the old key now names Ops' task, which a
    // member of Main alone still can't see.
    ok(
        app,
        "POST",
        &format!("/api/v1/workspaces/{}/prefix", w.ops.id),
        Some(json!({ "issue_prefix": "MAIN" })),
    )
    .await;
    let (status, view) = call(app, "GET", &format!("/api/v1/tasks/{old_key}"), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(view["workspace_id"], w.ops.id.as_str());
    let bob = member_token(&w.k.store, "bob", Role::Member).await;
    for (method, tail, body) in [
        ("GET", "", None),
        ("PATCH", "", Some(json!({ "title": "x" }))),
        ("POST", "/comments", Some(json!({ "body": "x" }))),
        ("GET", "/activity", None),
    ] {
        let path = format!("/api/v1/tasks/{old_key}{tail}");
        let (status, _) = call_as(app, &bob, method, &path, body).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{method} {path}");
    }
}

#[tokio::test]
async fn issue_prefixes_stay_unique_across_workspaces_under_races() {
    let k = kernel().await;
    let app = &k.app;
    let create = |name: &'static str| {
        ok(
            app,
            "POST",
            "/api/v1/workspaces",
            Some(json!({ "name": name })),
        )
    };
    let mainframe = create("Mainframe").await;
    assert_eq!(mainframe["issue_prefix"], "MAIN2");
    let main_two = create("Main 2").await;
    assert_eq!(main_two["issue_prefix"], "MAIN3");
    let prefix = |id: &Value| format!("/api/v1/workspaces/{}/prefix", id.as_str().unwrap());
    for taken in ["main", "Main2", " MAIN3 "] {
        let (status, error) = call(
            app,
            "POST",
            &prefix(&main_two["id"]),
            Some(json!({ "issue_prefix": taken })),
        )
        .await;
        if taken.trim().eq_ignore_ascii_case("MAIN3") {
            assert_eq!(status, StatusCode::OK, "its own prefix: {error}");
        } else {
            assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{taken}");
            let message = error["error"]["message"].as_str().unwrap();
            assert!(message.contains("already uses the prefix"), "{message}");
        }
    }
    // Two workspaces racing for one prefix: one wins, the other is told.
    for round in 0..5 {
        let wanted = format!("RACE{round}");
        let racers = [&mainframe["id"], &main_two["id"]].map(|id| {
            let (app, path, wanted) = (app.clone(), prefix(id), wanted.clone());
            tokio::spawn(async move {
                call(&app, "POST", &path, Some(json!({ "issue_prefix": wanted })))
                    .await
                    .0
            })
        });
        let mut statuses = Vec::new();
        for racer in racers {
            statuses.push(racer.await.unwrap());
        }
        statuses.sort();
        assert_eq!(
            statuses,
            [StatusCode::OK, StatusCode::UNPROCESSABLE_ENTITY],
            "round {round}"
        );
    }
}

/// A GitHub App delivery signed with `secret` over `signed`, sending `body`.
async fn github_delivery(
    app: &Router,
    secret: &str,
    signed: &str,
    body: &str,
    id: &str,
) -> StatusCode {
    let signature = format!(
        "sha256={}",
        hex(&hmac(secret.as_bytes(), signed.as_bytes()))
    );
    post(
        app,
        "/api/v1/webhooks/github",
        &[
            ("x-github-event", "issues"),
            ("x-github-delivery", id),
            ("x-hub-signature-256", &signature),
        ],
        body,
    )
    .await
}

const APP_WEBHOOK_SECRET: &str = "dummy-app-webhook-secret-1";

async fn github_app_kernel() -> (common::Kernel, Arc<Mods>) {
    let mods = Arc::new(Mods::default());
    let k = common::kernel_with_mods(mods.clone()).await;
    let mut tx = k.store.begin().await.unwrap();
    tx.set_secret(
        &sun_engine::ecosystem_scope().unwrap(),
        "GITHUB_APP_WEBHOOK_SECRET",
        APP_WEBHOOK_SECRET,
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    (k, mods)
}

#[tokio::test(flavor = "multi_thread")]
async fn forged_github_deliveries_are_refused() {
    let (k, mods) = github_app_kernel().await;
    let app = &k.app;
    let body = json!({ "action": "labeled", "issue": { "number": 1 } }).to_string();
    let other = json!({ "action": "labeled", "issue": { "number": 2 } }).to_string();
    // Signed over another body, with the wrong secret, the SHA-1 header
    // style, or nothing at all.
    assert_eq!(
        github_delivery(app, APP_WEBHOOK_SECRET, &other, &body, "d1").await,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        github_delivery(app, "dummy-wrong-secret", &body, &body, "d2").await,
        StatusCode::UNAUTHORIZED
    );
    let mac = hex(&hmac(APP_WEBHOOK_SECRET.as_bytes(), body.as_bytes()));
    for headers in [
        vec![("x-hub-signature-256", format!("sha1={mac}"))],
        vec![("x-hub-signature-256", String::new())],
        vec![("x-hub-signature-256", mac.clone())],
        vec![],
    ] {
        let headers: Vec<(&str, &str)> = headers.iter().map(|(n, v)| (*n, v.as_str())).collect();
        let mut all = vec![("x-github-event", "issues")];
        all.extend(headers.iter().copied());
        assert_eq!(
            post(app, "/api/v1/webhooks/github", &all, &body).await,
            StatusCode::UNAUTHORIZED,
            "{headers:?}"
        );
    }
    assert!(mods.0.lock().unwrap().is_empty(), "nothing reached the mod");
    assert_eq!(
        github_delivery(app, APP_WEBHOOK_SECRET, &body, &body, "d3").await,
        StatusCode::OK
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_replayed_github_delivery_reaches_the_mod_once() {
    let (k, mods) = github_app_kernel().await;
    let body = json!({ "action": "labeled", "issue": { "number": 1 } }).to_string();
    for _ in 0..2 {
        let status = github_delivery(&k.app, APP_WEBHOOK_SECRET, &body, &body, "same-id").await;
        assert!(status.is_success(), "{status}");
    }
    assert_eq!(mods.0.lock().unwrap().len(), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_failed_github_delivery_can_be_redelivered_with_its_id() {
    let (k, mods) = github_app_kernel().await;
    let body = json!({ "action": "labeled", "issue": { "number": 1 } }).to_string();
    mods.fail_next();
    let status = github_delivery(&k.app, APP_WEBHOOK_SECRET, &body, &body, "redo-id").await;
    assert!(!status.is_success(), "{status}");
    // GitHub's Redeliver keeps the id: the mod gets it this time, and once.
    for _ in 0..2 {
        let status = github_delivery(&k.app, APP_WEBHOOK_SECRET, &body, &body, "redo-id").await;
        assert!(status.is_success(), "{status}");
    }
    assert_eq!(
        mods.0.lock().unwrap().len(),
        2,
        "the failure and the redelivery"
    );
}

/// A kernel with GitLab connected on Main and on a new workspace Ops, a repo
/// (and hook) on each; the mods, the hook secrets (Main's first) and the
/// connection ids.
async fn two_gitlab_hooks() -> (common::Kernel, Arc<Mods>, Vec<String>, [String; 2]) {
    let mods = Arc::new(Mods::default());
    let k = common::kernel_with_mods(mods.clone()).await;
    let secrets = HookSecrets::default();
    let gitlab = gitlab_stub_seeing(secrets.clone());
    let ops = ok(
        &k.app,
        "POST",
        "/api/v1/workspaces",
        Some(json!({ "name": "Ops" })),
    )
    .await;
    let mut connections = Vec::new();
    for (workspace, project) in [(MAIN, "main-app"), (ops["id"].as_str().unwrap(), "ops-app")] {
        let connection = connect_gitlab(&k, workspace, &gitlab).await;
        connections.push(connection["id"].as_str().unwrap().to_owned());
        ok(
            &k.app,
            "POST",
            &format!("/api/v1/workspaces/{workspace}/repos"),
            Some(json!({ "url": format!("{gitlab}/acme/{project}.git") })),
        )
        .await;
    }
    let secrets = secrets.lock().unwrap().clone();
    assert_eq!(secrets.len(), 2, "one hook each");
    assert_ne!(secrets[0], secrets[1], "a secret per connection");
    let [main, ops]: [String; 2] = connections.try_into().unwrap();
    (k, mods, secrets, [main, ops])
}

async fn gitlab_delivery(app: &Router, connection: &str, token: &str, uuid: &str) -> StatusCode {
    post(
        app,
        &format!("/api/v1/webhooks/vcs/{connection}"),
        &[
            ("x-gitlab-event", "Note Hook"),
            ("x-gitlab-token", token),
            ("x-gitlab-event-uuid", uuid),
        ],
        &json!({ "object_kind": "note" }).to_string(),
    )
    .await
}

#[tokio::test(flavor = "multi_thread")]
async fn a_gitlab_hook_secret_opens_only_its_own_connection() {
    let (k, mods, secrets, [main, ops]) = two_gitlab_hooks().await;
    let app = &k.app;
    assert_eq!(
        gitlab_delivery(app, &ops, &secrets[0], "u1").await,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        gitlab_delivery(app, &main, &secrets[1], "u2").await,
        StatusCode::UNAUTHORIZED
    );
    // Neither the GitLab token nor a prefix of the secret opens it.
    assert_eq!(
        gitlab_delivery(app, &main, GITLAB_TOKEN, "u3").await,
        StatusCode::UNAUTHORIZED
    );
    let prefix = &secrets[0][..secrets[0].len() - 1];
    assert_eq!(
        gitlab_delivery(app, &main, prefix, "u4").await,
        StatusCode::UNAUTHORIZED
    );
    // The GitHub route doesn't take a GitLab hook secret either.
    let status = post(
        app,
        "/api/v1/webhooks/github",
        &[
            ("x-gitlab-token", &secrets[0]),
            ("x-github-event", "issues"),
        ],
        "{}",
    )
    .await;
    assert!(status.is_client_error(), "{status}");
    assert!(mods.0.lock().unwrap().is_empty());
    assert_eq!(
        gitlab_delivery(app, &main, &secrets[0], "u5").await,
        StatusCode::OK
    );
    assert_eq!(
        gitlab_delivery(app, &ops, &secrets[1], "u6").await,
        StatusCode::OK
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_replayed_gitlab_delivery_reaches_the_mod_once() {
    let (k, mods, secrets, [main, _]) = two_gitlab_hooks().await;
    for _ in 0..2 {
        let status = gitlab_delivery(&k.app, &main, &secrets[0], "same-uuid").await;
        assert!(status.is_success(), "{status}");
    }
    assert_eq!(mods.0.lock().unwrap().len(), 1);
}

/// A GitHub that knows installation 42 of account `acme`.
pub(crate) fn github_stub() -> String {
    let (url, _) = mock("", |r| {
        let auth = r.header("authorization").unwrap_or_default().to_owned();
        let jwt = auth.starts_with("Bearer ey") && auth.matches('.').count() == 2;
        match (r.method.as_str(), r.target.as_str()) {
            ("GET", "/app/installations/42") if jwt => {
                (200, json!({ "id": 42, "account": { "login": "acme" } }))
            }
            _ => (401, json!({ "message": "Bad credentials" })),
        }
    })
    .unwrap();
    url
}

/// The connect URL's state and the browser's cookie for it.
pub(crate) async fn github_connect(k: &common::Kernel) -> (String, String) {
    let request = Request::builder()
        .uri(format!("/api/v1/workspaces/{MAIN}/vcs/github/connect"))
        .header("authorization", format!("Bearer {TOKEN}"))
        .body(Body::empty())
        .unwrap();
    let response = common::respond(&k.app, request).await;
    assert_eq!(response.status(), StatusCode::OK);
    let cookie = response.headers()["set-cookie"].to_str().unwrap();
    let cookie = cookie.split(';').next().unwrap().to_owned();
    let bytes = http_body_util::BodyExt::collect(response.into_body())
        .await
        .unwrap()
        .to_bytes();
    let url: Value = serde_json::from_slice(&bytes).unwrap();
    let url = url["url"].as_str().unwrap();
    let state = url
        .split("state=")
        .nth(1)
        .unwrap()
        .split('&')
        .next()
        .unwrap();
    (state.replace("%2E", "."), cookie)
}

pub(crate) async fn setup(k: &common::Kernel, state: &str, cookie: &str) -> String {
    let request = Request::builder()
        .uri(format!(
            "/api/v1/vcs/github/setup?installation_id=42&state={state}"
        ))
        .header("cookie", cookie)
        .body(Body::empty())
        .unwrap();
    let response = common::respond(&k.app, request).await;
    response.headers()["location"].to_str().unwrap().to_owned()
}

async fn github_app_ready() -> common::Kernel {
    let k = kernel().await;
    let pem = include_str!("../../../sun-engine/src/vcs/test_app_key.pem");
    let mut tx = k.store.begin().await.unwrap();
    let scope = sun_engine::ecosystem_scope().unwrap();
    for (name, value) in [
        ("GITHUB_APP_ID", "123456"),
        ("GITHUB_APP_SLUG", "sun-dummy"),
        ("GITHUB_APP_PRIVATE_KEY", pem),
        ("GITHUB_APP_WEBHOOK_SECRET", APP_WEBHOOK_SECRET),
        ("GITHUB_APP_API_URL", &github_stub()),
    ] {
        tx.set_secret(&scope, name, value).await.unwrap();
    }
    tx.commit().await.unwrap();
    k
}

#[tokio::test(flavor = "multi_thread")]
async fn a_connect_state_is_bound_to_its_provider_browser_and_payload() {
    let k = github_app_ready().await;
    let (state, cookie) = github_connect(&k).await;
    let invalid = "/#/settings/code?github_error=invalid_state";
    // The payload swapped under the signature, the signature truncated, the
    // cookie of another provider, or the nonce in the wrong cookie.
    let (payload, mac) = state.split_once('.').unwrap();
    let mut changed: Value =
        serde_json::from_slice(&URL_SAFE_NO_PAD.decode(payload).unwrap()).unwrap();
    changed["actor"] = json!("user:mallory");
    let swapped = format!("{}.{mac}", URL_SAFE_NO_PAD.encode(changed.to_string()));
    let nonce = cookie.split_once('=').unwrap().1;
    for (bad_state, bad_cookie) in [
        (swapped, cookie.clone()),
        (
            format!("{payload}.{}", &mac[..mac.len() - 2]),
            cookie.clone(),
        ),
        (format!("{payload}."), cookie.clone()),
        (state.clone(), format!("sun_vcs_state_gitlab={nonce}")),
        (state.clone(), String::new()),
    ] {
        assert_eq!(
            setup(&k, &bad_state, &bad_cookie).await,
            invalid,
            "{bad_state} {bad_cookie}"
        );
    }
    assert!(
        setup(&k, &state, &cookie)
            .await
            .ends_with("github_connected=1")
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_used_connect_state_cannot_be_replayed() {
    let k = github_app_ready().await;
    let (state, cookie) = github_connect(&k).await;
    assert!(
        setup(&k, &state, &cookie)
            .await
            .ends_with("github_connected=1")
    );
    assert_eq!(
        setup(&k, &state, &cookie).await,
        "/#/settings/code?github_error=invalid_state"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn an_archived_agent_takes_no_task() {
    let w = world().await;
    let app = &w.k.app;
    ok(app, "DELETE", &format!("/api/v1/agents/{}", w.agent), None).await;
    let assignee = json!({ "kind": "agent", "id": w.agent });
    let path = format!("/api/v1/tasks/{}", w.main.plain);
    let (status, _) = call(app, "PATCH", &path, Some(json!({ "assignee": assignee }))).await;
    assert!(status.is_client_error(), "{status}");
    let new = json!({ "title": "x", "workspace_id": MAIN, "assignee": assignee });
    let (status, _) = call(app, "POST", "/api/v1/tasks", Some(new)).await;
    assert!(status.is_client_error(), "{status}");
}

#[tokio::test(flavor = "multi_thread")]
async fn an_agent_takes_only_its_workspaces_tasks_and_members_name_the_workspace() {
    let w = world().await;
    let app = &w.k.app;
    let assign = json!({ "assignee": { "kind": "agent", "id": w.agent } });
    let path = format!("/api/v1/tasks/{}", w.main.plain);
    ok(app, "PATCH", &path, Some(assign.clone())).await;
    // Main's agent is no assignee on Ops, even for a deployment owner.
    let path = format!("/api/v1/tasks/{}", w.ops.plain);
    let (status, _) = call_as(app, crate::common::TOKEN, "PATCH", &path, Some(assign)).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let bob = member_token(&w.k.store, "bob", Role::Member).await;
    let (status, _) = call_as(app, &bob, "GET", "/api/v1/agents", None).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    let of_main = "/api/v1/agents?workspace=00000000000000000000000001";
    let (status, agents) = call_as(app, &bob, "GET", of_main, None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(agents[0]["task_count"], 1, "{agents}");
}
