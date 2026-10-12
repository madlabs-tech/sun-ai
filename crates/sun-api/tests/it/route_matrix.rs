//! Every route the workspaces refactor added, against every kind of caller:
//! a workspace the caller can't see answers 404 (never 403), a missing
//! permission 403, and a viewer reads but never writes.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use serde_json::{Value, json};
use sun_core::GroupId;
use sun_mod_sdk::http::mock;

use http_body_util::BodyExt;
use sun_store::SecretRepo;

use crate::common::{self, Kernel, kernel};
use crate::workspaces::{FLOW, call, call_as, member_token};

pub(crate) const MAIN: &str = "00000000000000000000000001";
pub(crate) const PNG: &[u8] = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR";
/// A dummy GitLab token; the stub accepts only it.
pub(crate) const GITLAB_TOKEN: &str = "glpat-dummy-matrix-token-0001";

/// A loopback GitLab that trades any OAuth code for [`GITLAB_TOKEN`], knows
/// every `acme/*` project, and lets hooks be made, changed and deleted.
pub(crate) fn gitlab_stub() -> String {
    gitlab_stub_seeing(HookSecrets::default())
}

/// Every hook secret (`token`) Sun sent the stub, in order.
pub(crate) type HookSecrets = std::sync::Arc<std::sync::Mutex<Vec<String>>>;

/// [`gitlab_stub`], recording the hook secrets into `secrets`.
pub(crate) fn gitlab_stub_seeing(secrets: HookSecrets) -> String {
    let mut hook = 100;
    let (url, _) = mock("", move |r| {
        if r.method == "POST" && r.target == "/oauth/token" {
            return (200, oauth_grant());
        }
        if r.header("authorization") != Some(&format!("Bearer {GITLAB_TOKEN}")) {
            return (401, json!({ "message": "401 Unauthorized" }));
        }
        if let Some(token) = r.body["token"].as_str() {
            secrets.lock().unwrap().push(token.to_owned());
        }
        let target = r.target.as_str();
        let project = target
            .strip_prefix("/api/v4/projects/acme%2F")
            .map(|rest| rest.split('/').next().unwrap_or_default().to_owned());
        match (r.method.as_str(), project) {
            ("GET", None) if target == "/api/v4/user" => (200, json!({ "username": "ann" })),
            ("GET", None) if target.starts_with("/api/v4/projects") => (200, json!([])),
            ("GET", Some(name)) => (
                200,
                json!({ "id": 9, "path_with_namespace": format!("acme/{name}"),
                        "http_url_to_repo": format!("https://gitlab.test/acme/{name}.git"),
                        "default_branch": "main", "visibility": "private" }),
            ),
            ("POST", Some(_)) => {
                hook += 1;
                (201, json!({ "id": hook }))
            }
            ("PUT", Some(_)) => (200, json!({ "id": hook })),
            ("DELETE", Some(_)) => (204, json!({})),
            _ => (404, json!({ "message": "404 Not Found" })),
        }
    })
    .unwrap();
    url
}

/// What a GitLab stub answers to `POST /oauth/token`: [`GITLAB_TOKEN`].
pub(crate) fn oauth_grant() -> Value {
    json!({ "access_token": GITLAB_TOKEN, "refresh_token": "glrt-dummy-refresh-0001",
            "expires_in": 7200 })
}

/// Connects `workspace` to the GitLab at `gitlab` through the OAuth flow
/// (the operator's app secrets are set; the stub trades any code for
/// [`GITLAB_TOKEN`]); the connection as `GET /workspaces/{id}/vcs` lists it.
pub(crate) async fn connect_gitlab(k: &Kernel, workspace: &str, gitlab: &str) -> Value {
    let mut tx = k.store.begin().await.unwrap();
    let scope = sun_engine::ecosystem_scope().unwrap();
    for (name, value) in [
        ("GITLAB_OAUTH_URL", gitlab),
        ("GITLAB_OAUTH_CLIENT_ID", "dummy-client-id"),
        ("GITLAB_OAUTH_CLIENT_SECRET", "dummy-gitlab-oauth-secret-7"),
    ] {
        tx.set_secret(&scope, name, value).await.unwrap();
    }
    tx.commit().await.unwrap();
    let request = Request::builder()
        .uri(format!("/api/v1/workspaces/{workspace}/vcs/gitlab/connect"))
        .header(header::AUTHORIZATION, format!("Bearer {}", common::TOKEN))
        .body(Body::empty())
        .unwrap();
    let response = common::respond(&k.app, request).await;
    assert_eq!(response.status(), StatusCode::OK);
    let cookie = response.headers()[header::SET_COOKIE].to_str().unwrap();
    let cookie = cookie.split(';').next().unwrap().to_owned();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let answer: Value = serde_json::from_slice(&bytes).unwrap();
    let url = answer["url"].as_str().expect("configured");
    let state = url
        .split("state=")
        .nth(1)
        .unwrap()
        .split('&')
        .next()
        .unwrap();
    let state = state.replace("%2E", ".");
    let request = Request::builder()
        .uri(format!(
            "/api/v1/vcs/gitlab/callback?code=dummy-code&state={state}"
        ))
        .header(header::COOKIE, cookie)
        .body(Body::empty())
        .unwrap();
    let response = common::respond(&k.app, request).await;
    let location = response.headers()[header::LOCATION].to_str().unwrap();
    assert!(location.contains("gitlab_connected=1"), "{location}");
    let listed = ok(
        &k.app,
        "GET",
        &format!("/api/v1/workspaces/{workspace}/vcs"),
        None,
    )
    .await;
    listed["connections"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["provider"] == "gitlab" && c["instance_url"] == gitlab)
        .cloned()
        .expect("the GitLab connection")
}

/// A raw-body request as `token`; the status.
pub(crate) async fn raw(
    app: &Router,
    token: &str,
    method: &str,
    path: &str,
    content_type: &str,
    body: Vec<u8>,
) -> StatusCode {
    let request = Request::builder()
        .method(method)
        .uri(path)
        .header(header::AUTHORIZATION, format!("Bearer {token}"))
        .header(header::CONTENT_TYPE, content_type)
        .body(Body::from(body))
        .unwrap();
    common::respond(app, request).await.status()
}

pub(crate) async fn ok(app: &Router, method: &str, path: &str, body: Option<Value>) -> Value {
    let (status, reply) = call(app, method, path, body).await;
    assert!(status.is_success(), "{method} {path}: {status} {reply}");
    reply
}

/// Waits until the task reaches `status`; its view.
pub(crate) async fn until(app: &Router, task: &str, status: &str) -> Value {
    for _ in 0..300 {
        let (_, view) = call(app, "GET", &format!("/api/v1/tasks/{task}"), None).await;
        if view["status"] == status {
            return view;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("task {task} never reached {status}");
}

/// What one workspace holds for the matrix.
pub(crate) struct Side {
    pub id: String,
    pub project: String,
    /// A plain task's key.
    pub plain: String,
    /// A workflow task's key and its run.
    pub flow_task: String,
    pub run: String,
    pub connection: String,
    pub repo: String,
}

/// Main (seen by the seeded groups) and Ops (no groups: hidden from all but
/// admins), each with a logo, a project, tasks, a run, a GitLab connection
/// and a repo.
pub(crate) async fn side(k: &Kernel, gitlab: &str, id: &str, name: &str) -> Side {
    let app = &k.app;
    assert_eq!(
        raw(
            app,
            common::TOKEN,
            "PUT",
            &format!("/api/v1/workspaces/{id}/logo"),
            "image/png",
            PNG.to_vec()
        )
        .await,
        StatusCode::OK
    );
    let project = ok(
        app,
        "POST",
        &format!("/api/v1/workspaces/{id}/projects"),
        Some(json!({ "name": "Web" })),
    )
    .await;
    let plain = ok(
        app,
        "POST",
        "/api/v1/tasks",
        Some(json!({ "title": "plain", "workspace_id": id })),
    )
    .await;
    let workflow = ok(
        app,
        "POST",
        "/api/v1/workflows",
        Some(json!({ "source_yaml": FLOW, "workspace_id": id })),
    )
    .await;
    let flow_task = ok(
        app,
        "POST",
        "/api/v1/tasks",
        Some(json!({ "title": "flow", "workflow_id": workflow["id"] })),
    )
    .await;
    let key = flow_task["key"].as_str().unwrap().to_owned();
    let run = until(app, &key, "needs_you").await["run"]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let connection = connect_gitlab(k, id, gitlab).await;
    let repo = ok(
        app,
        "POST",
        &format!("/api/v1/workspaces/{id}/repos"),
        Some(json!({ "url": format!("{gitlab}/acme/{name}.git") })),
    )
    .await;
    Side {
        id: id.to_owned(),
        project: project["id"].as_str().unwrap().to_owned(),
        plain: plain["key"].as_str().unwrap().to_owned(),
        flow_task: key,
        run,
        connection: connection["id"].as_str().unwrap().to_owned(),
        repo: repo["repo"]["id"].as_str().unwrap().to_owned(),
    }
}

pub(crate) struct World {
    pub k: Kernel,
    pub main: Side,
    pub ops: Side,
    pub agent: String,
}

pub(crate) async fn world() -> World {
    let k = kernel().await;
    let gitlab = gitlab_stub();
    let ops = ok(
        &k.app,
        "POST",
        "/api/v1/workspaces",
        Some(json!({ "name": "Ops" })),
    )
    .await;
    let ops = ops["id"].as_str().unwrap().to_owned();
    let main = side(&k, &gitlab, MAIN, "main-app").await;
    let ops = side(&k, &gitlab, &ops, "ops-app").await;
    let agent = ok(
        &k.app,
        "POST",
        "/api/v1/agents",
        Some(json!({ "name": "Ada", "runtime_chain": ["claude-code"] })),
    )
    .await;
    World {
        k,
        main,
        ops,
        agent: agent["id"].as_str().unwrap().to_owned(),
    }
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum Need {
    /// Seeing the workspace.
    Read,
    /// `run`.
    Run,
    /// `edit_workspace`.
    Edit,
    /// `edit_workspace` + `manage_secrets`.
    EditSecrets,
}

/// `(method, path, body, need)` for every new workspace-scoped route of `s`,
/// the ones that change or delete things last.
fn routes(s: &Side, agent: &str) -> Vec<(&'static str, String, Option<Value>, Need)> {
    let ws = format!("/api/v1/workspaces/{}", s.id);
    let task = format!("/api/v1/tasks/{}", s.plain);
    let flow = format!("/api/v1/tasks/{}", s.flow_task);
    let assign = json!({ "assignee": { "kind": "agent", "id": agent } });
    vec![
        ("GET", format!("{ws}/logo"), None, Need::Read),
        ("GET", format!("{ws}/projects"), None, Need::Read),
        ("GET", format!("{ws}/vcs"), None, Need::Read),
        ("GET", format!("{ws}/repos"), None, Need::Read),
        ("GET", task.clone(), None, Need::Read),
        ("GET", format!("{task}/pull_requests"), None, Need::Read),
        ("GET", format!("{flow}/activity"), None, Need::Read),
        ("GET", format!("{flow}/runs"), None, Need::Read),
        ("GET", format!("/api/v1/runs/{}", s.run), None, Need::Read),
        (
            "GET",
            format!("{ws}/vcs/{}/repos", s.connection),
            None,
            Need::Edit,
        ),
        (
            "GET",
            format!("{ws}/vcs/github/connect"),
            None,
            Need::EditSecrets,
        ),
        (
            "GET",
            format!("{ws}/vcs/gitlab/connect"),
            None,
            Need::EditSecrets,
        ),
        (
            "POST",
            format!("/api/v1/vcs/connections/{}/rotate-webhook", s.connection),
            None,
            Need::EditSecrets,
        ),
        (
            "PATCH",
            ws.clone(),
            Some(json!({ "description": "d" })),
            Need::Edit,
        ),
        (
            "POST",
            format!("{ws}/prefix"),
            Some(json!({ "issue_prefix": "main" })),
            Need::Edit,
        ),
        (
            "POST",
            format!("{ws}/repos"),
            Some(json!({ "url": "https://gitlab.example/acme/x.git" })),
            Need::Edit,
        ),
        (
            "PATCH",
            format!("/api/v1/repos/{}", s.repo),
            Some(json!({ "description": "d" })),
            Need::Edit,
        ),
        (
            "POST",
            format!("{ws}/projects"),
            Some(json!({ "name": "New" })),
            Need::Run,
        ),
        (
            "PATCH",
            format!("/api/v1/projects/{}", s.project),
            Some(json!({ "description": "d" })),
            Need::Run,
        ),
        (
            "PATCH",
            task.clone(),
            Some(json!({ "status": "in_progress" })),
            Need::Run,
        ),
        ("PATCH", task.clone(), Some(assign), Need::Run),
        (
            "PUT",
            format!("{task}/pr_auto_complete"),
            Some(json!({ "disabled": true })),
            Need::Run,
        ),
        (
            "PATCH",
            task.clone(),
            Some(json!({ "project_id": s.project })),
            Need::Run,
        ),
        (
            "POST",
            format!("{task}/comments"),
            Some(json!({ "body": "hi" })),
            Need::Run,
        ),
        ("POST", format!("{flow}/interrupt"), None, Need::Run),
        ("DELETE", format!("{ws}/logo"), None, Need::Edit),
        ("DELETE", format!("{ws}/repos/{}", s.repo), None, Need::Edit),
        (
            "DELETE",
            format!("/api/v1/projects/{}", s.project),
            None,
            Need::Run,
        ),
        (
            "DELETE",
            format!("/api/v1/vcs/connections/{}", s.connection),
            None,
            Need::EditSecrets,
        ),
    ]
}

async fn hit(
    app: &Router,
    token: &str,
    method: &str,
    path: &str,
    body: Option<Value>,
) -> StatusCode {
    if method == "GET" && path.ends_with("/logo") {
        return raw(app, token, method, path, "", Vec::new()).await;
    }
    call_as(app, token, method, path, body).await.0
}

#[tokio::test(flavor = "multi_thread")]
async fn every_new_route_hides_other_workspaces_and_checks_its_permission() {
    let w = world().await;
    let app = &w.k.app;
    // An editor: sees Main (its group is attached) and may edit it, but has
    // no `manage_secrets`.
    let editors = ok(
        app,
        "POST",
        "/api/v1/groups",
        Some(json!({ "name": "Editors", "permissions": ["edit_workspace", "run"] })),
    )
    .await;
    let editors: GroupId = editors["id"].as_str().unwrap().parse().unwrap();
    let all = json!({ "group_ids": [GroupId::VIEWER, GroupId::MEMBER, GroupId::ADMIN, editors] });
    ok(
        app,
        "PUT",
        &format!("/api/v1/workspaces/{MAIN}/groups"),
        Some(all),
    )
    .await;
    let viewer = member_token(&w.k.store, "vic", &[GroupId::VIEWER]).await;
    let member = member_token(&w.k.store, "bob", &[GroupId::VIEWER, GroupId::MEMBER]).await;
    let editor = member_token(&w.k.store, "eve", &[GroupId::VIEWER, editors]).await;

    // Ops is hidden from everyone but admins: 404 for every route, whatever
    // the permission, even the reads.
    for token in [&viewer, &member, &editor] {
        for (method, path, body, _) in routes(&w.ops, &w.agent) {
            let status = hit(app, token, method, &path, body).await;
            assert_eq!(status, StatusCode::NOT_FOUND, "{token}: {method} {path}");
        }
    }

    // On Main: the viewer reads, writes nothing.
    for (method, path, body, need) in routes(&w.main, &w.agent) {
        let status = hit(app, &viewer, method, &path, body).await;
        let want = if need == Need::Read {
            StatusCode::OK
        } else {
            StatusCode::FORBIDDEN
        };
        assert_eq!(status, want, "viewer: {method} {path}");
    }
    // The member lacks `edit_workspace` and `manage_secrets`.
    for (method, path, body, need) in routes(&w.main, &w.agent) {
        if matches!(need, Need::Edit | Need::EditSecrets) {
            let status = hit(app, &member, method, &path, body).await;
            assert_eq!(status, StatusCode::FORBIDDEN, "member: {method} {path}");
        }
    }
    // The editor lacks only `manage_secrets`.
    for (method, path, body, need) in routes(&w.main, &w.agent) {
        if need == Need::EditSecrets {
            let status = hit(app, &editor, method, &path, body).await;
            assert_eq!(status, StatusCode::FORBIDDEN, "editor: {method} {path}");
        }
    }
    // Agents: reads are open, writes need `manage_runtimes`.
    let agent = format!("/api/v1/agents/{}", w.agent);
    assert_eq!(
        call_as(app, &viewer, "GET", "/api/v1/agents", None).await.0,
        StatusCode::OK
    );
    let writes = [
        (
            "POST",
            "/api/v1/agents".to_owned(),
            Some(json!({ "name": "Bo", "runtime_chain": ["codex"] })),
        ),
        ("PATCH", agent.clone(), Some(json!({ "description": "x" }))),
        ("DELETE", agent.clone(), None),
    ];
    for token in [&viewer, &member, &editor] {
        for (method, path, body) in writes.clone() {
            let status = call_as(app, token, method, &path, body).await.0;
            assert_eq!(status, StatusCode::FORBIDDEN, "{method} {path}");
        }
    }

    // With the permission, each route does its job on Main (whatever its
    // own answer), never a 401, 403 or 404 from the guards.
    for (method, path, body, need) in routes(&w.main, &w.agent) {
        let token = match need {
            Need::Read => &viewer,
            Need::Run => &member,
            Need::Edit => &editor,
            Need::EditSecrets => common::TOKEN,
        };
        let status = hit(app, token, method, &path, body).await;
        assert!(
            !matches!(
                status,
                StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN | StatusCode::NOT_FOUND
            ),
            "{method} {path}: {status}"
        );
    }
}

/// `GET /tasks?assignee=` and `?project=` never list another workspace's
/// tasks, and a project of a hidden workspace can't be named on a visible task.
#[tokio::test(flavor = "multi_thread")]
async fn filters_and_bodies_never_reach_into_a_hidden_workspace() {
    let w = world().await;
    let app = &w.k.app;
    let assign = json!({ "assignee": { "kind": "agent", "id": w.agent } });
    for side in [&w.main, &w.ops] {
        ok(
            app,
            "PATCH",
            &format!("/api/v1/tasks/{}", side.plain),
            Some(assign.clone()),
        )
        .await;
    }
    let member = member_token(&w.k.store, "bob", &[GroupId::VIEWER, GroupId::MEMBER]).await;
    let (status, listed) = call_as(
        app,
        &member,
        "GET",
        &format!("/api/v1/tasks?assignee={}", w.agent),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let keys: Vec<&str> = listed
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["key"].as_str().unwrap())
        .collect();
    assert_eq!(keys, [w.main.plain.as_str()]);
    let by_project = format!("/api/v1/tasks?project={}", w.ops.project);
    let (status, listed) = call_as(app, &member, "GET", &by_project, None).await;
    assert!(
        status == StatusCode::NOT_FOUND || listed == json!([]),
        "{status} {listed}"
    );
    let sneak = json!({ "project_id": w.ops.project });
    let path = format!("/api/v1/tasks/{}", w.main.plain);
    let (status, _) = call_as(app, &member, "PATCH", &path, Some(sneak)).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}
