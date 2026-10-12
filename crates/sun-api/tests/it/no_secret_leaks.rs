//! Git tokens, webhook secrets and the GitHub App's private key never show
//! in an API answer, the audit log, the outbox, an SSE stream or a task's
//! activity: through the normal flows, and with a git host that echoes the
//! credentials it was sent back in its errors. Dummy values only.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, header};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use sun_mod_sdk::http::mock;
use sun_store::SecretRepo;

use crate::abuse::{github_connect, github_stub, hex, hmac, post, setup};
use crate::common::{self, TOKEN, kernel};
use crate::route_matrix::{
    GITLAB_TOKEN, HookSecrets, MAIN, connect_gitlab, gitlab_stub_seeing, oauth_grant, ok, until,
};
use crate::workspaces::{FLOW, call};

const PEM: &str = include_str!("../../../sun-engine/src/vcs/test_app_key.pem");
const APP_WEBHOOK_SECRET: &str = "dummy-app-webhook-secret-7";
const OAUTH_SECRET: &str = "dummy-gitlab-oauth-secret-7";

/// A mod link that takes every delivery and says nothing.
struct Quiet;

#[async_trait::async_trait]
impl sun_core::ModLink for Quiet {
    async fn call(&self, _: sun_core::ModCall) -> sun_core::Result<Value> {
        Ok(json!({}))
    }
}

/// A line of the PEM body: enough to tell the key leaked.
fn pem_line() -> &'static str {
    PEM.lines().nth(1).unwrap()
}

/// The text of a GET as admin (any status).
async fn text(app: &Router, path: &str, accept: &str) -> String {
    let request = Request::builder()
        .uri(path)
        .header(header::AUTHORIZATION, format!("Bearer {TOKEN}"))
        .header(header::ACCEPT, accept)
        .body(Body::empty())
        .unwrap();
    let response = common::respond(app, request).await;
    let mut body = response.into_body();
    let mut out = Vec::new();
    // An SSE stream never ends: read what comes within a moment.
    let wait = Duration::from_millis(300);
    while let Ok(Some(Ok(frame))) = tokio::time::timeout(wait, body.frame()).await {
        if let Some(data) = frame.data_ref() {
            out.extend_from_slice(data);
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn assert_clean(what: &str, text: &str, secrets: &[String]) {
    for secret in secrets {
        assert!(
            !text.contains(secret.as_str()),
            "{what} shows a secret ({}…): {text}",
            &secret[..6.min(secret.len())]
        );
    }
}

async fn set_ecosystem(k: &common::Kernel, pairs: &[(&str, &str)]) {
    let mut tx = k.store.begin().await.unwrap();
    let scope = sun_engine::ecosystem_scope().unwrap();
    for (name, value) in pairs {
        tx.set_secret(&scope, name, value).await.unwrap();
    }
    tx.commit().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn no_token_secret_or_key_in_any_answer_audit_outbox_or_stream() {
    let k = common::kernel_with_mods(std::sync::Arc::new(Quiet)).await;
    let app = &k.app;
    set_ecosystem(
        &k,
        &[
            ("GITHUB_APP_ID", "123456"),
            ("GITHUB_APP_SLUG", "sun-dummy"),
            ("GITHUB_APP_PRIVATE_KEY", PEM),
            ("GITHUB_APP_WEBHOOK_SECRET", APP_WEBHOOK_SECRET),
            ("GITHUB_APP_API_URL", &github_stub()),
            ("GITLAB_OAUTH_CLIENT_ID", "dummy-client-id"),
            ("GITLAB_OAUTH_CLIENT_SECRET", OAUTH_SECRET),
        ],
    )
    .await;
    let hooks = HookSecrets::default();
    let gitlab = gitlab_stub_seeing(hooks.clone());
    let mut answers = Vec::new();
    let connection = connect_gitlab(&k, MAIN, &gitlab).await;
    answers.push(connection.to_string());
    let id = connection["id"].as_str().unwrap();
    let repo = ok(
        app,
        "POST",
        &format!("/api/v1/workspaces/{MAIN}/repos"),
        Some(json!({ "url": format!("{gitlab}/acme/app.git") })),
    )
    .await;
    answers.push(repo.to_string());
    let rotated = ok(
        app,
        "POST",
        &format!("/api/v1/vcs/connections/{id}/rotate-webhook"),
        None,
    )
    .await;
    answers.push(rotated.to_string());
    for provider in ["github", "gitlab"] {
        let path = format!("/api/v1/workspaces/{MAIN}/vcs/{provider}/connect");
        answers.push(ok(app, "GET", &path, None).await.to_string());
    }
    // A workflow task with a run, comments and moves.
    let workflow = ok(
        app,
        "POST",
        "/api/v1/workflows",
        Some(json!({ "source_yaml": FLOW })),
    )
    .await;
    let task = ok(
        app,
        "POST",
        "/api/v1/tasks",
        Some(json!({ "title": "t", "workflow_id": workflow["id"] })),
    )
    .await;
    let key = task["key"].as_str().unwrap().to_owned();
    let run = until(app, &key, "needs_you").await["run"]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    ok(
        app,
        "POST",
        &format!("/api/v1/tasks/{key}/comments"),
        Some(json!({ "body": "looks fine" })),
    )
    .await;
    // GitHub connected too, and a PR of its installation mirrored onto the
    // task (signed with the App's secret), so the connection list and the
    // PR sidebar have something to leak.
    let (state, cookie) = github_connect(&k).await;
    assert!(
        setup(&k, &state, &cookie)
            .await
            .ends_with("github_connected=1")
    );
    let delivery = json!({
        "action": "opened",
        "installation": { "id": 42 },
        "repository": { "name": "web", "owner": { "login": "acme" } },
        "pull_request": {
            "number": 7, "title": format!("{key} looks fine"), "body": "", "state": "open",
            "draft": false, "merged": false, "html_url": "https://github.com/acme/web/pull/7",
            "head": { "ref": "fix", "sha": "abc" }, "user": { "login": "ada", "avatar_url": null },
            "created_at": "2026-10-07T06:00:00Z", "updated_at": "2026-10-07T07:00:00Z",
            "merged_at": null, "closed_at": null, "additions": 1, "deletions": 0, "changed_files": 1
        }
    })
    .to_string();
    let signature = format!(
        "sha256={}",
        hex(&hmac(APP_WEBHOOK_SECRET.as_bytes(), delivery.as_bytes()))
    );
    let status = post(
        app,
        "/api/v1/webhooks/github",
        &[
            ("x-github-event", "pull_request"),
            ("x-github-delivery", "leak-7"),
            ("x-hub-signature-256", &signature),
        ],
        &delivery,
    )
    .await;
    assert!(status.is_success(), "{status}");
    let mirrored = ok(
        app,
        "GET",
        &format!("/api/v1/tasks/{key}/pull_requests"),
        None,
    )
    .await;
    assert_eq!(mirrored["pull_requests"][0]["number"], 7, "{mirrored}");
    answers.push(mirrored.to_string());

    let mut secrets = vec![
        GITLAB_TOKEN.to_owned(),
        APP_WEBHOOK_SECRET.to_owned(),
        OAUTH_SECRET.to_owned(),
        pem_line().to_owned(),
    ];
    secrets.extend(hooks.lock().unwrap().iter().cloned());
    assert!(
        secrets.len() >= 6,
        "the hook and its rotated secret: {secrets:?}"
    );

    for (i, answer) in answers.iter().enumerate() {
        assert_clean(&format!("write answer {i}"), answer, &secrets);
    }
    let json = "application/json";
    let sse = "text/event-stream";
    for (path, accept) in [
        ("/api/v1/workspaces".to_owned(), json),
        (format!("/api/v1/workspaces/{MAIN}/vcs"), json),
        (format!("/api/v1/workspaces/{MAIN}/vcs/{id}/repos"), json),
        (format!("/api/v1/workspaces/{MAIN}/repos"), json),
        ("/api/v1/audit".to_owned(), json),
        ("/api/v1/outbox".to_owned(), json),
        ("/api/v1/secrets".to_owned(), json),
        ("/api/v1/tasks".to_owned(), json),
        (format!("/api/v1/tasks/{key}"), json),
        (format!("/api/v1/tasks/{key}/activity"), json),
        (format!("/api/v1/tasks/{key}/activity"), sse),
        (format!("/api/v1/tasks/{key}/runs"), json),
        (format!("/api/v1/tasks/{key}/pull_requests"), json),
        (format!("/api/v1/runs/{run}"), json),
        (format!("/api/v1/runs/{run}/events"), json),
        (format!("/api/v1/runs/{run}/events"), sse),
        ("/api/v1/agents".to_owned(), json),
        ("/api/v1/me".to_owned(), json),
    ] {
        let body = text(app, &path, accept).await;
        assert_clean(&format!("GET {path} ({accept})"), &body, &secrets);
    }
    // The context pack agents get lists the repo, never its credentials.
    let (_, pack) = call(
        app,
        "POST",
        &format!("/api/v1/tasks/{key}/context/preview"),
        Some(json!({})),
    )
    .await;
    assert_clean("the context pack", &pack.to_string(), &secrets);
}

/// A GitLab that takes [`GITLAB_TOKEN`] for `/user` and projects, then
/// fails every hook call and every listing, echoing what it was sent:
/// the `Authorization` header and the body (with the hook secret).
fn echoing_gitlab() -> String {
    let (url, _) = mock("", |r| {
        if r.method == "POST" && r.target == "/oauth/token" {
            return (200, oauth_grant());
        }
        let auth = r.header("authorization").unwrap_or_default().to_owned();
        if auth != format!("Bearer {GITLAB_TOKEN}") {
            return (401, json!({ "message": "401 Unauthorized" }));
        }
        let echo = json!({ "message": format!("refused {auth} {}", r.body) });
        match (r.method.as_str(), r.target.as_str()) {
            ("GET", "/api/v4/user") => (200, json!({ "username": "ann" })),
            ("GET", t) if t.starts_with("/api/v4/projects/acme%2F") && !t.contains("/hooks") => (
                200,
                json!({ "id": 9, "path_with_namespace": "acme/app",
                        "http_url_to_repo": "https://gitlab.test/acme/app.git",
                        "default_branch": "main" }),
            ),
            _ => (500, echo),
        }
    })
    .unwrap();
    url
}

#[tokio::test(flavor = "multi_thread")]
async fn a_git_host_echoing_credentials_in_its_errors_never_leaks_them() {
    let k = kernel().await;
    let app = &k.app;
    let gitlab = echoing_gitlab();
    let connection = connect_gitlab(&k, MAIN, &gitlab).await;
    let id = connection["id"].as_str().unwrap();
    // The webhook secret Sun made for this connection, read from the store.
    let hook_secret = {
        let mut tx = k.store.read().await.unwrap();
        let scope = sun_core::model::SecretScope::Workspace(sun_core::WorkspaceId::MAIN);
        let names = tx.list_secrets(Some(&scope)).await.unwrap();
        let mut values = Vec::new();
        for secret in names {
            values.push(tx.secret_value(&scope, &secret.name).await.unwrap());
        }
        values
    };
    let mut secrets = vec![GITLAB_TOKEN.to_owned()];
    secrets.extend(hook_secret);
    assert!(secrets.len() >= 2, "{secrets:?}");

    let mut seen = Vec::new();
    let (_, added) = call(
        app,
        "POST",
        &format!("/api/v1/workspaces/{MAIN}/repos"),
        Some(json!({ "url": format!("{gitlab}/acme/app.git") })),
    )
    .await;
    // The manual-hook panel shows the secret once, by design; nothing else may.
    let mut added_rest = added.clone();
    if let Some(hook) = added_rest
        .get_mut("manual_hook")
        .and_then(Value::as_object_mut)
    {
        hook.remove("secret");
    }
    seen.push(("add repo", added_rest.to_string()));
    let (_, rotated) = call(
        app,
        "POST",
        &format!("/api/v1/vcs/connections/{id}/rotate-webhook"),
        None,
    )
    .await;
    let mut rotated_rest = rotated.clone();
    if let Some(hook) = rotated_rest
        .get_mut("manual_hook")
        .and_then(Value::as_object_mut)
    {
        hook.remove("secret");
    }
    seen.push(("rotate", rotated_rest.to_string()));
    let (_, listed) = call(
        app,
        "GET",
        &format!("/api/v1/workspaces/{MAIN}/vcs/{id}/repos?q=x"),
        None,
    )
    .await;
    seen.push(("picker", listed.to_string()));
    for (what, body) in &seen {
        assert_clean(what, body, &secrets);
    }
    // The secret after the rotation is a secret too.
    let mut tx = k.store.read().await.unwrap();
    let scope = sun_core::model::SecretScope::Workspace(sun_core::WorkspaceId::MAIN);
    for secret in tx.list_secrets(Some(&scope)).await.unwrap() {
        secrets.push(tx.secret_value(&scope, &secret.name).await.unwrap());
    }
    drop(tx);
    // Disconnecting queues hook deletions the host refuses (echoing): the
    // outbox keeps the errors, never the credentials.
    if let Some(repo) = added["repo"]["id"].as_str() {
        let (status, _) = call(
            app,
            "DELETE",
            &format!("/api/v1/workspaces/{MAIN}/repos/{repo}"),
            None,
        )
        .await;
        assert!(status.is_success(), "{status}");
    }
    tokio::time::sleep(Duration::from_millis(1500)).await;
    for path in ["/api/v1/outbox", "/api/v1/audit"] {
        assert_clean(path, &text(app, path, "application/json").await, &secrets);
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_stored_secret_pasted_into_a_comment_is_redacted_from_the_activity() {
    let k = kernel().await;
    let app = &k.app;
    set_ecosystem(&k, &[("GITHUB_APP_WEBHOOK_SECRET", APP_WEBHOOK_SECRET)]).await;
    let task = ok(
        app,
        "POST",
        "/api/v1/tasks",
        Some(json!({ "title": "t", "workspace_id": MAIN })),
    )
    .await;
    let key = task["key"].as_str().unwrap();
    let pasted = format!("oops, the secret is {APP_WEBHOOK_SECRET}");
    let (_, comment) = call(
        app,
        "POST",
        &format!("/api/v1/tasks/{key}/comments"),
        Some(json!({ "body": pasted })),
    )
    .await;
    let secrets = [APP_WEBHOOK_SECRET.to_owned()];
    assert_clean("the comment answer", &comment.to_string(), &secrets);
    let path = format!("/api/v1/tasks/{key}/activity");
    for accept in ["application/json", "text/event-stream"] {
        assert_clean(&path, &text(app, &path, accept).await, &secrets);
    }
}
