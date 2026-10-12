//! Pull requests through the real kernel: a GitHub App `pull_request`
//! delivery is mirrored (and still forwarded to the mod) and links the task
//! its branch names; a stale redelivery changes nothing; a person links a
//! PR by URL (404 until Sun received it), removes one (it stays removed),
//! and turns the merge rule off; merging every linked PR moves the task; a
//! GitLab MR hook mirrors into its workspace; a viewer can't link; and no
//! secret shows in any answer.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use axum::Router;
use axum::http::StatusCode;
use serde_json::{Value, json};
use sun_core::{GroupId, ModCall, ModLink, Result};
use sun_store::SecretRepo;

use crate::abuse::{github_connect, github_stub, hex, hmac, post, setup};
use crate::common::{self, TOKEN};
use crate::route_matrix::{HookSecrets, MAIN, connect_gitlab, gitlab_stub_seeing, ok};
use crate::workspaces::{call, call_as, member_token};

const APP_WEBHOOK_SECRET: &str = "dummy-app-webhook-secret-pr";

/// Records the deliveries; answers `{}`.
#[derive(Default)]
struct Mods(Mutex<Vec<ModCall>>);

#[async_trait]
impl ModLink for Mods {
    async fn call(&self, call: ModCall) -> Result<Value> {
        self.0.lock().unwrap().push(call);
        Ok(json!({}))
    }
}

/// A kernel with the GitHub App set up (a stub knows installation 42 of
/// `acme`) and Main connected to it.
async fn github_kernel() -> (common::Kernel, Arc<Mods>) {
    let mods = Arc::new(Mods::default());
    let k = common::kernel_with_mods(mods.clone()).await;
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
    let (state, cookie) = github_connect(&k).await;
    let landed = setup(&k, &state, &cookie).await;
    assert!(landed.ends_with("github_connected=1"), "{landed}");
    (k, mods)
}

/// A GitHub `pull_request` payload for `acme/web#number` on `branch`.
fn pull_request(number: u64, title: &str, branch: &str, updated: &str) -> Value {
    json!({
        "action": "opened",
        "installation": { "id": 42 },
        "repository": { "name": "web", "owner": { "login": "acme" } },
        "pull_request": {
            "number": number, "title": title, "body": "", "state": "open", "draft": false,
            "merged": false, "html_url": format!("https://github.com/acme/web/pull/{number}"),
            "head": { "ref": branch, "sha": format!("sha{number}") },
            "user": { "login": "ada", "avatar_url": null },
            "created_at": "2026-10-07T06:00:00Z", "updated_at": updated,
            "merged_at": null, "closed_at": null,
            "additions": 10, "deletions": 2, "changed_files": 1
        }
    })
}

/// Delivers `payload` signed with the App's secret, as delivery `id`.
async fn deliver(app: &Router, event: &str, payload: &Value, id: &str) -> StatusCode {
    let body = payload.to_string();
    let signature = format!(
        "sha256={}",
        hex(&hmac(APP_WEBHOOK_SECRET.as_bytes(), body.as_bytes()))
    );
    post(
        app,
        "/api/v1/webhooks/github",
        &[
            ("x-github-event", event),
            ("x-github-delivery", id),
            ("x-hub-signature-256", &signature),
        ],
        &body,
    )
    .await
}

/// `GET /tasks/{key}/pull_requests`.
async fn prs(app: &Router, key: &str) -> Value {
    ok(
        app,
        "GET",
        &format!("/api/v1/tasks/{key}/pull_requests"),
        None,
    )
    .await
}

fn numbers(view: &Value) -> Vec<u64> {
    view["pull_requests"]
        .as_array()
        .unwrap()
        .iter()
        .map(|pr| pr["number"].as_u64().unwrap())
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn github_deliveries_mirror_prs_link_tasks_and_move_them_on_merge() {
    let (k, mods) = github_kernel().await;
    let app = &k.app;
    let task = ok(
        app,
        "POST",
        "/api/v1/tasks",
        Some(json!({ "title": "Fix login", "workspace_id": MAIN })),
    )
    .await;
    let key = task["key"].as_str().unwrap().to_owned();
    let branch = format!("sun/{key}-fix-login");
    let bob = member_token(&k.store, "bob", &[GroupId::VIEWER, GroupId::MEMBER]).await;
    let vic = member_token(&k.store, "vic", &[GroupId::VIEWER]).await;
    let mut answers: Vec<String> = Vec::new();

    // Nothing yet.
    let view = prs(app, &key).await;
    assert_eq!(view["pull_requests"], json!([]));
    assert_eq!(view["auto_complete"]["state"], "none");

    // A PR on the task's branch: mirrored, linked by the branch, and still
    // handed to the mod.
    let seven = pull_request(7, "Login fix", &branch, "2026-10-07T09:00:00Z");
    assert_eq!(
        deliver(app, "pull_request", &seven, "d-7").await,
        StatusCode::OK
    );
    let view = prs(app, &key).await;
    answers.push(view.to_string());
    assert_eq!(numbers(&view), [7], "{view:#}");
    let pr = &view["pull_requests"][0];
    assert_eq!(pr["link_source"], "branch");
    assert_eq!(pr["provider"], "github");
    assert_eq!(pr["repo_owner"], "acme");
    assert_eq!(pr["state"], "open");
    assert_eq!(pr["html_url"], "https://github.com/acme/web/pull/7");
    assert_eq!(pr["additions"], 10);
    assert_eq!(view["auto_complete"]["state"], "waiting");
    assert_eq!(view["auto_complete"]["target_status"], "needs_you");
    assert_eq!(view["auto_complete"]["workspace_enabled"], true);
    {
        let calls = mods.0.lock().unwrap();
        assert_eq!(calls.len(), 1, "forwarded once");
        assert_eq!(calls[0].params["event"], "pull_request");
    }
    let seven_id = pr["id"].as_str().unwrap().to_owned();

    // The same delivery again: dropped before the mirror and the mod. An
    // older edit (a redelivery out of order): a no-op on the mirror.
    assert_eq!(
        deliver(app, "pull_request", &seven, "d-7").await,
        StatusCode::OK
    );
    let stale = pull_request(7, "Old title", &branch, "2026-10-07T08:00:00Z");
    assert_eq!(
        deliver(app, "pull_request", &stale, "d-7-old").await,
        StatusCode::OK
    );
    let view = prs(app, &key).await;
    assert_eq!(view["pull_requests"][0]["title"], "Login fix", "{view:#}");
    assert_eq!(mods.0.lock().unwrap().len(), 2);

    // A PR naming no task: mirrored, linked to nothing, until a person
    // pastes its URL (any case, with a query) on the task. One Sun never
    // received is not found, with the copy the UI shows.
    let eight = pull_request(8, "Pricing table", "pricing-table", "2026-10-07T10:00:00Z");
    assert_eq!(
        deliver(app, "pull_request", &eight, "d-8").await,
        StatusCode::OK
    );
    assert_eq!(numbers(&prs(app, &key).await), [7]);
    let link = format!("/api/v1/tasks/{key}/pull_requests");
    let (status, missing) = call_as(
        app,
        &bob,
        "POST",
        &link,
        Some(json!({ "url": "https://github.com/acme/web/pull/99" })),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{missing}");
    assert!(
        missing["error"]["message"]
            .as_str()
            .unwrap()
            .contains("it appears here once its repository is connected"),
        "{missing}"
    );
    let (status, bad) = call_as(app, &bob, "POST", &link, Some(json!({}))).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{bad}");
    let (status, view) = call_as(
        app,
        &bob,
        "POST",
        &link,
        Some(json!({ "url": "https://github.com/ACME/Web/pull/8?diff=split" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{view}");
    answers.push(view.to_string());
    assert_eq!(numbers(&view), [7, 8], "{view:#}");
    assert_eq!(view["pull_requests"][1]["link_source"], "manual");
    let eight_id = view["pull_requests"][1]["id"].as_str().unwrap().to_owned();

    // A viewer (no `run`) and the CLI token (no member) change nothing.
    let (status, _) = call_as(
        app,
        &vic,
        "POST",
        &link,
        Some(json!({ "pull_request_id": eight_id })),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, _) = call_as(app, &vic, "DELETE", &format!("{link}/{seven_id}"), None).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, _) = call(app, "DELETE", &format!("{link}/{seven_id}"), None).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "the token holder is nobody");
    let (status, read) = call_as(app, &vic, "GET", &link, None).await;
    assert_eq!((status, numbers(&read)), (StatusCode::OK, vec![7, 8]));

    // Removing #7 remembers it: its next delivery doesn't link it again.
    let (status, view) = call_as(app, &bob, "DELETE", &format!("{link}/{seven_id}"), None).await;
    assert_eq!(status, StatusCode::OK, "{view}");
    assert_eq!(numbers(&view), [8]);
    let edited = pull_request(7, "Login fix, again", &branch, "2026-10-07T11:00:00Z");
    assert_eq!(
        deliver(app, "pull_request", &edited, "d-7-edit").await,
        StatusCode::OK
    );
    assert_eq!(numbers(&prs(app, &key).await), [8]);
    let (status, _) = call_as(app, &bob, "DELETE", &format!("{link}/{seven_id}"), None).await;
    assert_eq!(status, StatusCode::OK, "removing twice is fine");

    // The task's own switch: off, the merge rule stands down.
    let switch = format!("/api/v1/tasks/{key}/pr_auto_complete");
    let (status, view) =
        call_as(app, &bob, "PUT", &switch, Some(json!({ "disabled": true }))).await;
    assert_eq!(status, StatusCode::OK, "{view}");
    assert_eq!(view["auto_complete"]["state"], "task_disabled");
    assert_eq!(view["auto_complete"]["task_disabled"], true);
    let (_, view) = call_as(
        app,
        &bob,
        "PUT",
        &switch,
        Some(json!({ "disabled": false })),
    )
    .await;
    assert_eq!(view["auto_complete"]["state"], "waiting");
    assert_eq!(view["auto_complete"]["pull_request_ids"], json!([eight_id]));

    // #8 merges: every linked PR is merged, so the task (no run: a person
    // confirms) moves to Needs you, logged as PR automation.
    let mut merged = pull_request(8, "Pricing table", "pricing-table", "2026-10-07T12:00:00Z");
    merged["action"] = json!("closed");
    merged["pull_request"]["state"] = json!("closed");
    merged["pull_request"]["merged"] = json!(true);
    merged["pull_request"]["merged_at"] = json!("2026-10-07T12:00:00Z");
    merged["pull_request"]["closed_at"] = json!("2026-10-07T12:00:00Z");
    assert_eq!(
        deliver(app, "pull_request", &merged, "d-8-merged").await,
        StatusCode::OK
    );
    let (_, task) = call(app, "GET", &format!("/api/v1/tasks/{key}"), None).await;
    assert_eq!(task["status"], "needs_you", "{task:#}");
    let view = prs(app, &key).await;
    answers.push(view.to_string());
    assert_eq!(view["pull_requests"][0]["state"], "merged");
    assert_eq!(view["auto_complete"]["state"], "at_target", "{view:#}");
    let (_, activity) = call(app, "GET", &format!("/api/v1/tasks/{key}/activity"), None).await;
    let moved = activity
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["kind"] == "status" && a["body"]["source"] == "pr_automation")
        .unwrap_or_else(|| panic!("{activity:#}"));
    assert_eq!(moved["body"]["to"], "needs_you");
    assert_eq!(moved["body"]["pull_requests"], "#8");
    assert!(
        activity
            .as_array()
            .unwrap()
            .iter()
            .any(|a| a["kind"] == "pull_request" && a["body"]["action"] == "linked"),
        "{activity:#}"
    );

    // Secrets never show: not the App's, not in any PR answer.
    let listed = ok(app, "GET", &format!("/api/v1/workspaces/{MAIN}/vcs"), None).await;
    answers.push(listed.to_string());
    answers.push(activity.to_string());
    for answer in &answers {
        assert!(!answer.contains(APP_WEBHOOK_SECRET), "{answer}");
        assert!(!answer.contains("BEGIN"), "{answer}");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn gitlab_merge_request_hooks_mirror_into_their_workspace() {
    let mods = Arc::new(Mods::default());
    let k = common::kernel_with_mods(mods.clone()).await;
    let app = &k.app;
    let secrets = HookSecrets::default();
    let gitlab = gitlab_stub_seeing(secrets.clone());
    let connection = connect_gitlab(&k, MAIN, &gitlab).await;
    let id = connection["id"].as_str().unwrap().to_owned();
    // The repo's hook carries the connection's secret.
    ok(
        app,
        "POST",
        &format!("/api/v1/workspaces/{MAIN}/repos"),
        Some(json!({ "url": format!("{gitlab}/acme/app.git") })),
    )
    .await;
    let secret = secrets.lock().unwrap()[0].clone();
    let task = ok(
        app,
        "POST",
        "/api/v1/tasks",
        Some(json!({ "title": "Retry the refresh", "workspace_id": MAIN })),
    )
    .await;
    let key = task["key"].as_str().unwrap().to_owned();

    let hook = json!({
        "object_kind": "merge_request",
        "user": { "username": "ann", "avatar_url": null },
        "project": { "path_with_namespace": "acme/app" },
        "object_attributes": {
            "iid": 3, "title": format!("{key} Retry the session refresh"), "description": "",
            "state": "opened", "draft": false, "work_in_progress": false,
            "url": "https://gitlab.test/acme/app/-/merge_requests/3", "source_branch": "retry",
            "last_commit": { "id": "def456" },
            "created_at": "2026-10-07T06:00:00Z", "updated_at": "2026-10-07T09:00:00Z"
        }
    });
    let status = post(
        app,
        &format!("/api/v1/webhooks/vcs/{id}"),
        &[
            ("x-gitlab-event", "Merge Request Hook"),
            ("x-gitlab-token", &secret),
            ("x-gitlab-event-uuid", "mr-3"),
        ],
        &hook.to_string(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let view = ok(
        app,
        "GET",
        &format!("/api/v1/tasks/{key}/pull_requests"),
        None,
    )
    .await;
    let text = view.to_string();
    assert_eq!(numbers(&view), [3], "{view:#}");
    let mr = &view["pull_requests"][0];
    assert_eq!(mr["provider"], "gitlab");
    assert_eq!(mr["repo_owner"], "acme");
    assert_eq!(mr["repo_name"], "app");
    assert_eq!(mr["link_source"], "title");
    assert_eq!(mr["branch"], "retry");
    assert_eq!(mr["author_login"], "ann");
    assert!(!text.contains(&secret), "{text}");
    assert!(!text.contains(crate::route_matrix::GITLAB_TOKEN), "{text}");
    // Forwarded to the mod too.
    {
        let calls = mods.0.lock().unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].params["event"], "Merge Request Hook");
    }

    // A wrong token mirrors nothing (and reaches no mod).
    let status = post(
        app,
        &format!("/api/v1/webhooks/vcs/{id}"),
        &[
            ("x-gitlab-event", "Merge Request Hook"),
            ("x-gitlab-token", "nope-nope"),
        ],
        &hook.to_string(),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let (status, _) = call_as(app, TOKEN, "GET", "/api/v1/me", None).await;
    assert_eq!(status, StatusCode::OK);

    // A Pipeline Hook at the MR's head is its one check; at another commit
    // it is nobody's.
    let pipeline = |sha: &str, status: &str, uuid: &str| {
        let app = k.app.clone();
        let hook = json!({
            "object_kind": "pipeline",
            "project": { "path_with_namespace": "acme/app" },
            "object_attributes": { "sha": sha, "status": status }
        })
        .to_string();
        let (id, secret, uuid) = (id.clone(), secret.clone(), uuid.to_owned());
        async move {
            post(
                &app,
                &format!("/api/v1/webhooks/vcs/{id}"),
                &[
                    ("x-gitlab-event", "Pipeline Hook"),
                    ("x-gitlab-token", &secret),
                    ("x-gitlab-event-uuid", &uuid),
                ],
                &hook,
            )
            .await
        }
    };
    let checks = |view: &Value| {
        let pr = &view["pull_requests"][0];
        (
            pr["checks_rollup_state"].clone(),
            pr["checks_total"].as_i64().unwrap(),
            pr["checks_passed"].as_i64().unwrap(),
            pr["checks_failed"].as_i64().unwrap(),
            pr["checks_pending"].as_i64().unwrap(),
            pr["snapshot_available"].as_bool().unwrap(),
        )
    };
    let path = format!("/api/v1/tasks/{key}/pull_requests");
    assert_eq!(
        checks(&ok(app, "GET", &path, None).await),
        (Value::Null, 0, 0, 0, 0, false)
    );
    assert_eq!(pipeline("def456", "running", "p-1").await, StatusCode::OK);
    assert_eq!(
        checks(&ok(app, "GET", &path, None).await),
        (json!("PENDING"), 1, 0, 0, 1, true)
    );
    assert_eq!(pipeline("def456", "success", "p-2").await, StatusCode::OK);
    assert_eq!(
        checks(&ok(app, "GET", &path, None).await),
        (json!("SUCCESS"), 1, 1, 0, 0, true)
    );
    assert_eq!(pipeline("0ther", "failed", "p-3").await, StatusCode::OK);
    assert_eq!(
        checks(&ok(app, "GET", &path, None).await),
        (json!("SUCCESS"), 1, 1, 0, 0, true),
        "another commit's pipeline"
    );
    assert_eq!(pipeline("def456", "failed", "p-4").await, StatusCode::OK);
    assert_eq!(
        checks(&ok(app, "GET", &path, None).await),
        (json!("FAILURE"), 1, 0, 1, 0, true)
    );
}

/// An `installation` delivery with `action` for installation 42.
fn installation(action: &str) -> Value {
    json!({ "action": action, "installation": { "id": 42, "account": { "login": "acme" } } })
}

#[tokio::test(flavor = "multi_thread")]
async fn an_uninstalled_or_suspended_app_forgets_the_connection_but_keeps_its_prs() {
    let (k, _mods) = github_kernel().await;
    let app = &k.app;
    let vcs = format!("/api/v1/workspaces/{MAIN}/vcs");
    let task = ok(
        app,
        "POST",
        "/api/v1/tasks",
        Some(json!({ "title": "Fix login", "workspace_id": MAIN })),
    )
    .await;
    let key = task["key"].as_str().unwrap().to_owned();
    let branch = format!("sun/{key}-fix-login");
    let seven = pull_request(7, "Login fix", &branch, "2026-10-07T09:00:00Z");
    assert_eq!(
        deliver(app, "pull_request", &seven, "d-7").await,
        StatusCode::OK
    );
    assert_eq!(numbers(&prs(app, &key).await), [7]);

    // Suspended: the connection goes; the PR and its link stay.
    assert_eq!(
        deliver(app, "installation", &installation("suspend"), "i-1").await,
        StatusCode::OK
    );
    assert_eq!(ok(app, "GET", &vcs, None).await["connections"], json!([]));
    let view = prs(app, &key).await;
    assert_eq!(numbers(&view), [7], "{view:#}");
    assert_eq!(view["pull_requests"][0]["link_source"], "branch");
    // With no connection, the installation's deliveries mirror nowhere.
    let edited = pull_request(7, "Login fix, renamed", &branch, "2026-10-07T10:00:00Z");
    assert_eq!(
        deliver(app, "pull_request", &edited, "d-7-renamed").await,
        StatusCode::OK
    );
    assert_eq!(
        prs(app, &key).await["pull_requests"][0]["title"],
        "Login fix"
    );

    // Reconnected, then uninstalled: the same.
    let (state, cookie) = github_connect(&k).await;
    assert!(
        setup(&k, &state, &cookie)
            .await
            .ends_with("github_connected=1")
    );
    assert_eq!(
        ok(app, "GET", &vcs, None).await["connections"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        deliver(app, "installation", &installation("deleted"), "i-2").await,
        StatusCode::OK
    );
    assert_eq!(ok(app, "GET", &vcs, None).await["connections"], json!([]));
    assert_eq!(numbers(&prs(app, &key).await), [7]);
    // Other installation actions change nothing.
    assert_eq!(
        deliver(app, "installation", &installation("created"), "i-3").await,
        StatusCode::OK
    );
    assert_eq!(ok(app, "GET", &vcs, None).await["connections"], json!([]));
}

#[tokio::test(flavor = "multi_thread")]
async fn pull_request_writes_need_a_member_with_the_task_write_permission() {
    let k = common::kernel().await;
    let app = &k.app;
    let task = ok(
        app,
        "POST",
        "/api/v1/tasks",
        Some(json!({ "title": "Fix login", "workspace_id": MAIN })),
    )
    .await;
    let key = task["key"].as_str().unwrap();
    let link = format!("/api/v1/tasks/{key}/pull_requests");
    let switch = format!("/api/v1/tasks/{key}/pr_auto_complete");
    let vic = member_token(&k.store, "vic", &[GroupId::VIEWER]).await;
    let bob = member_token(&k.store, "bob", &[GroupId::VIEWER, GroupId::MEMBER]).await;
    let pr = sun_core::PullRequestId::generate().to_string();
    let url = json!({ "url": "https://github.com/acme/web/pull/1" });
    let off = json!({ "disabled": true });
    // A viewer reads, and changes nothing; the CLI token (admin, but no
    // member to record) changes nothing either.
    let (status, _) = call_as(app, &vic, "GET", &link, None).await;
    assert_eq!(status, StatusCode::OK);
    for (who, token) in [("viewer", vic.as_str()), ("token", TOKEN)] {
        for (method, path, body) in [
            ("POST", link.clone(), Some(url.clone())),
            ("DELETE", format!("{link}/{pr}"), None),
            ("PUT", switch.clone(), Some(off.clone())),
        ] {
            let (status, reply) = call_as(app, token, method, &path, body).await;
            assert_eq!(
                status,
                StatusCode::FORBIDDEN,
                "{who} {method} {path}: {reply}"
            );
        }
    }
    // A member with `run` may; the task's switch moves, the unknown PR is 404.
    let (status, view) = call_as(app, &bob, "PUT", &switch, Some(off)).await;
    assert_eq!(status, StatusCode::OK, "{view}");
    assert_eq!(view["auto_complete"]["task_disabled"], true);
    let (status, _) = call_as(app, &bob, "POST", &link, Some(url)).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _) = call_as(app, &bob, "DELETE", &format!("{link}/{pr}"), None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _) = call_as(app, &bob, "DELETE", &format!("{link}/not-an-id"), None).await;
    assert_eq!(
        status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "a malformed id is 422"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn deliveries_over_ten_mib_are_refused_before_anything_reads_them() {
    let (k, mods) = github_kernel().await;
    let app = &k.app;
    let limit = 10 * 1024 * 1024;
    // `{"pad":"xxx…"}` of exactly `size` bytes.
    let body_of = |size: usize| format!("{{\"pad\":\"{}\"}}", "x".repeat(size - 10));
    let over = body_of(limit + 1);
    let signature = format!(
        "sha256={}",
        hex(&hmac(APP_WEBHOOK_SECRET.as_bytes(), over.as_bytes()))
    );
    let status = post(
        app,
        "/api/v1/webhooks/github",
        &[
            ("x-github-event", "pull_request"),
            ("x-github-delivery", "big-1"),
            ("x-hub-signature-256", &signature),
        ],
        &over,
    )
    .await;
    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);
    let status = post(
        app,
        &format!(
            "/api/v1/webhooks/vcs/{}",
            sun_core::VcsConnectionId::generate()
        ),
        &[
            ("x-gitlab-event", "Merge Request Hook"),
            ("x-gitlab-token", "x"),
        ],
        &over,
    )
    .await;
    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);
    assert!(mods.0.lock().unwrap().is_empty());
    // At the limit the body is read: an unsigned one is 401, a signed one
    // (no PR in it) is forwarded.
    let at = body_of(limit);
    let status = post(
        app,
        "/api/v1/webhooks/github",
        &[("x-github-event", "pull_request")],
        &at,
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let signature = format!(
        "sha256={}",
        hex(&hmac(APP_WEBHOOK_SECRET.as_bytes(), at.as_bytes()))
    );
    let status = post(
        app,
        "/api/v1/webhooks/github",
        &[
            ("x-github-event", "pull_request"),
            ("x-github-delivery", "big-2"),
            ("x-hub-signature-256", &signature),
        ],
        &at,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(mods.0.lock().unwrap().len(), 1);
}
