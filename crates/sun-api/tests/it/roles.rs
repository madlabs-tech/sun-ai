//! The role matrix in a workspace other than Main: viewer, member, admin and
//! owner against every kind of write there, each route with the lowest role
//! that may use it. A role below gets 403; a role at or above gets past the
//! guards (its own answer, never 401, 403 or 404). The deployment's own
//! routes stay 403 for the workspace's owner; the token holder passes all.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use axum::http::StatusCode;
use serde_json::{Value, json};
use sun_core::WorkspaceId;
use sun_core::model::Role;
use sun_store::MemberRepo;

use crate::common::kernel;
use crate::workspaces::{FLOW, call, call_as, member_token_in};

/// The lowest role a route needs; `None`: deployment owners only.
type Min = Option<Role>;

const MEMBER: Min = Some(Role::Member);
const ADMIN: Min = Some(Role::Admin);
const DEPLOYMENT: Min = None;

async fn ok(path: &str, value: (StatusCode, Value)) -> Value {
    assert!(value.0.is_success(), "{path}: {} {}", value.0, value.1);
    value.1
}

#[tokio::test(flavor = "multi_thread")]
async fn roles_each_write_in_a_workspace_needs_its_role_there() {
    let k = kernel().await;
    let app = &k.app;
    let w = ok(
        "ws",
        call(
            app,
            "POST",
            "/api/v1/workspaces",
            Some(json!({ "name": "Ops" })),
        )
        .await,
    )
    .await["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let w_id: WorkspaceId = w.parse().unwrap();
    let task = ok(
        "task",
        call(
            app,
            "POST",
            "/api/v1/tasks",
            Some(json!({ "title": "t", "workspace_id": w })),
        )
        .await,
    )
    .await["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let flow = json!({ "source_yaml": FLOW, "workspace_id": w });
    let workflow = ok(
        "wf",
        call(app, "POST", "/api/v1/workflows", Some(flow)).await,
    )
    .await["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let projects = format!("/api/v1/workspaces/{w}/projects");
    let project = ok(
        "p",
        call(app, "POST", &projects, Some(json!({ "name": "P" }))).await,
    )
    .await["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let agents = format!("/api/v1/agents?workspace={w}");
    let agent = json!({ "name": "Ada", "runtime_chain": ["claude-code"] });
    let agent = ok("agent", call(app, "POST", &agents, Some(agent)).await).await["id"]
        .as_str()
        .unwrap()
        .to_owned();
    // Somebody each role may change (to the role they have: no change).
    member_token_in(&k.store, "target", &[(w_id, Role::Viewer)]).await;
    let target = {
        let mut read = k.store.read().await.unwrap();
        let member = read
            .member_by_email("target@corp.io")
            .await
            .unwrap()
            .unwrap();
        member.id.to_string()
    };

    // `name` makes each role's writes distinct, so none collides with another's.
    let routes = |name: &str| -> Vec<(&'static str, String, Option<Value>, Min)> {
        let ws = format!("/api/v1/workspaces/{w}");
        vec![
            // Run.
            (
                "POST",
                "/api/v1/tasks".into(),
                Some(json!({ "title": name, "workspace_id": w })),
                MEMBER,
            ),
            (
                "PATCH",
                format!("/api/v1/tasks/{task}"),
                Some(json!({ "title": name })),
                MEMBER,
            ),
            (
                "POST",
                format!("/api/v1/tasks/{task}/comments"),
                Some(json!({ "body": name })),
                MEMBER,
            ),
            (
                "PUT",
                format!("/api/v1/tasks/{task}/budget"),
                Some(json!({ "cost_usd": 5.0 })),
                MEMBER,
            ),
            (
                "POST",
                format!("{ws}/projects"),
                Some(json!({ "name": name })),
                MEMBER,
            ),
            (
                "PATCH",
                format!("/api/v1/projects/{project}"),
                Some(json!({ "description": name })),
                MEMBER,
            ),
            // Approve.
            (
                "POST",
                format!("/api/v1/tasks/{task}/approve"),
                Some(json!({})),
                MEMBER,
            ),
            // Create and edit workflows.
            (
                "POST",
                "/api/v1/workflows".into(),
                Some(json!({ "workspace_id": w, "source_yaml":
                FLOW.replace("name: ship", &format!("name: ship-{name}")) })),
                MEMBER,
            ),
            (
                "POST",
                format!("/api/v1/workflows/{workflow}/versions"),
                Some(json!({ "source_yaml": FLOW })),
                MEMBER,
            ),
            (
                "POST",
                format!("/api/v1/templates/bug-fix/install?workspace={w}"),
                None,
                MEMBER,
            ),
            // The workspace itself.
            (
                "PATCH",
                ws.clone(),
                Some(json!({ "description": name })),
                ADMIN,
            ),
            (
                "POST",
                format!("{ws}/repos"),
                Some(json!({ "url": format!("https://example.com/{name}") })),
                ADMIN,
            ),
            ("GET", format!("{ws}/vcs/github/connect"), None, ADMIN),
            // Members and invitations.
            ("GET", format!("{ws}/invitations"), None, ADMIN),
            (
                "POST",
                format!("{ws}/invitations"),
                Some(json!({ "email": format!("{name}-guest@corp.io"), "role": "viewer" })),
                ADMIN,
            ),
            (
                "PUT",
                format!("{ws}/members/{target}"),
                Some(json!({ "role": "viewer" })),
                ADMIN,
            ),
            // Agents, runtimes, hosts.
            (
                "POST",
                format!("/api/v1/agents?workspace={w}"),
                Some(json!({ "name": name, "runtime_chain": ["codex"] })),
                ADMIN,
            ),
            (
                "PATCH",
                format!("/api/v1/agents/{agent}"),
                Some(json!({ "description": name })),
                ADMIN,
            ),
            (
                "POST",
                format!("/api/v1/runtimes?workspace={w}"),
                Some(json!({ "name": name,
                "source": "byo_key", "config": { "endpoint": "http://localhost:11434/v1",
                "model": "qwen" } })),
                ADMIN,
            ),
            ("POST", format!("{ws}/hosts/tokens"), None, ADMIN),
            // Plugins.
            (
                "POST",
                format!("/api/v1/catalog/tdd/install?workspace={w}"),
                Some(json!({})),
                ADMIN,
            ),
            (
                "DELETE",
                format!("/api/v1/catalog/tdd?workspace={w}"),
                None,
                ADMIN,
            ),
            (
                "POST",
                format!("/api/v1/mcp_servers?workspace={w}"),
                Some(json!({ "name": "sun", "transport": { "type": "url", "url": "https://x" } })),
                ADMIN,
            ),
            // Secrets.
            ("GET", format!("/api/v1/secrets?workspace={w}"), None, ADMIN),
            (
                "PUT",
                format!("/api/v1/secrets/workspace/{w}/KEY_{}", name.to_uppercase()),
                Some(json!({ "value": "sk-0123456789abcdef" })),
                ADMIN,
            ),
            // The deployment's own, whatever the role here.
            (
                "GET",
                format!("/api/v1/members?workspace={w}"),
                None,
                DEPLOYMENT,
            ),
            (
                "GET",
                format!("/api/v1/audit?workspace={w}"),
                None,
                DEPLOYMENT,
            ),
            (
                "GET",
                format!("/api/v1/kernel?workspace={w}"),
                None,
                DEPLOYMENT,
            ),
            (
                "PUT",
                format!("/api/v1/settings/on_limit?workspace={w}"),
                Some(json!({ "on_limit": null, "workspace_id": w })),
                DEPLOYMENT,
            ),
        ]
    };

    let mut seen = 0;
    for role in [Role::Viewer, Role::Member, Role::Admin, Role::Owner] {
        let name = role.as_str();
        let token = member_token_in(&k.store, &format!("as_{name}"), &[(w_id, role)]).await;
        for (method, path, body, min) in routes(name) {
            let (status, value) = call_as(app, &token, method, &path, body).await;
            let allowed = min.is_some_and(|min| role.covers(min));
            if allowed {
                assert!(
                    !matches!(
                        status,
                        StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN | StatusCode::NOT_FOUND
                    ),
                    "{role}: {method} {path} -> {status} {value}"
                );
            } else {
                assert_eq!(
                    status,
                    StatusCode::FORBIDDEN,
                    "{role}: {method} {path} {value}"
                );
            }
            seen += 1;
        }
    }
    for (method, path, body, _) in routes("token") {
        let (status, value) = call(app, method, &path, body).await;
        assert!(
            !matches!(
                status,
                StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN | StatusCode::NOT_FOUND
            ),
            "token: {method} {path} -> {status} {value}"
        );
    }
    assert_eq!(seen, 4 * routes("x").len());

    // Deleting the workspace: admin and owner only (each on a spare one).
    for (role, want) in [
        (Role::Viewer, StatusCode::FORBIDDEN),
        (Role::Member, StatusCode::FORBIDDEN),
        (Role::Admin, StatusCode::NO_CONTENT),
        (Role::Owner, StatusCode::NO_CONTENT),
    ] {
        let spare = ok(
            "spare",
            call(
                app,
                "POST",
                "/api/v1/workspaces",
                Some(json!({ "name": format!("Spare {role}") })),
            )
            .await,
        )
        .await["id"]
            .as_str()
            .unwrap()
            .parse::<WorkspaceId>()
            .unwrap();
        let token = member_token_in(&k.store, &format!("del_{role}"), &[(spare, role)]).await;
        let path = format!("/api/v1/workspaces/{spare}");
        let (status, value) = call_as(app, &token, "DELETE", &path, None).await;
        assert_eq!(status, want, "{role}: {value}");
    }
}

/// `who: [role:admin]` is the role in the task's workspace: an admin of
/// Main who is a member of Ops can't approve an Ops gate; an Ops admin can.
#[tokio::test(flavor = "multi_thread")]
async fn roles_an_approval_role_is_the_role_in_the_tasks_workspace() {
    let k = kernel().await;
    let app = &k.app;
    let ops = ok(
        "ws",
        call(
            app,
            "POST",
            "/api/v1/workspaces",
            Some(json!({ "name": "Ops" })),
        )
        .await,
    )
    .await["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let ops_id: WorkspaceId = ops.parse().unwrap();
    let host = format!("/api/v1/workspaces/{ops}/hosts/{}", k.host);
    ok("host", call(app, "PUT", &host, None).await).await;
    let flow = json!({ "source_yaml": FLOW, "workspace_id": ops });
    let workflow = ok(
        "wf",
        call(app, "POST", "/api/v1/workflows", Some(flow)).await,
    )
    .await;
    let task = json!({ "title": "gate", "workflow_id": workflow["id"] });
    let task = ok("task", call(app, "POST", "/api/v1/tasks", Some(task)).await).await;
    let key = task["key"].as_str().unwrap().to_owned();
    crate::route_matrix::until(app, &key, "needs_you").await;

    let main_admin = member_token_in(
        &k.store,
        "mia",
        &[(WorkspaceId::MAIN, Role::Admin), (ops_id, Role::Member)],
    )
    .await;
    let approve = format!("/api/v1/tasks/{key}/approve");
    let (status, value) = call_as(app, &main_admin, "POST", &approve, Some(json!({}))).await;
    assert!(!status.is_success(), "a member of Ops: {status} {value}");
    let (_, view) = call(app, "GET", &format!("/api/v1/tasks/{key}"), None).await;
    assert_eq!(view["status"], "needs_you", "{view}");
    let ops_admin = member_token_in(&k.store, "oda", &[(ops_id, Role::Admin)]).await;
    let (status, value) = call_as(app, &ops_admin, "POST", &approve, Some(json!({}))).await;
    assert_eq!(status, StatusCode::OK, "{value}");
}
