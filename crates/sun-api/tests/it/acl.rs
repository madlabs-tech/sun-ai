//! Workspace roles through the real kernel: a role grants fixed
//! permissions, a workspace is visible to its members only, invites grant
//! at most the inviter's role, and the take-over guard keeps members off
//! anyone who can do or see more. Until M2 checks the role in the workspace
//! a route names, a member's permissions are their role in Main.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use crate::common;

use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use common::{TOKEN, kernel};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use sun_core::model::Role;
use sun_core::model::{OutboxItem, OutboxStatus};
use sun_core::{IdemKey, OutboxId, RunId, StepKey, WorkspaceId};
use sun_store::{LoginRepo, MemberRepo, MembershipRepo, OutboxRepo};

async fn call_as(
    app: &Router,
    token: &str,
    method: &str,
    path: &str,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let request = Request::builder()
        .method(method)
        .uri(path)
        .header(header::AUTHORIZATION, format!("Bearer {token}"))
        .header(header::CONTENT_TYPE, "application/json")
        .body(body.map_or(Body::empty(), |b| Body::from(b.to_string())))
        .unwrap();
    let response = common::respond(app, request).await;
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

/// A session token of a new member with these `roles`, and their id.
async fn member(
    store: &sun_store::Store,
    name: &str,
    roles: &[(WorkspaceId, Role)],
) -> (String, String) {
    let token = format!("session-{name}");
    let hash: String = Sha256::digest(token.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    let email = format!("{name}@corp.io");
    let mut tx = store.begin().await.unwrap();
    let member = tx
        .upsert_member("https://idp", name, name, Some(&email), false)
        .await
        .unwrap();
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

const MAIN: WorkspaceId = WorkspaceId::MAIN;

/// Workspace `name`, made by the token holder: its id.
async fn workspace(app: &Router, name: &str) -> WorkspaceId {
    let body = json!({ "name": name });
    let (status, workspace) = call_as(app, TOKEN, "POST", "/api/v1/workspaces", Some(body)).await;
    assert_eq!(status, StatusCode::CREATED, "{workspace}");
    id_of(&workspace).parse().unwrap()
}

/// `id` of a JSON answer.
fn id_of(value: &Value) -> String {
    value["id"].as_str().unwrap().to_owned()
}

/// Does the JSON list hold an item with `id`?
fn lists(list: &Value, id: &str) -> bool {
    list.as_array().unwrap().iter().any(|item| item["id"] == id)
}

/// A one-step workflow named `name`.
fn flow(name: &str) -> String {
    format!("name: {name}\nsteps:\n  - id: ok\n    type: approval\n    who: [role:admin]\n")
}

#[tokio::test]
async fn acl_roles_grant_their_permissions_and_anyone_creates_a_workspace() {
    let k = kernel().await;
    let app = &k.app;
    let (bob, _) = member(&k.store, "bob", &[(MAIN, Role::Member)]).await;
    let (status, me) = call_as(app, &bob, "GET", "/api/v1/me", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        me["permissions"],
        json!(["run", "approve", "create_workflow", "edit_workflow"])
    );
    assert_eq!(me["memberships"][0]["role"], "member", "{me}");
    // A viewer reads, writes nothing, but creates a workspace of their own
    // and owns it.
    let (vic, vic_id) = member(&k.store, "vic", &[(MAIN, Role::Viewer)]).await;
    let (_, me) = call_as(app, &vic, "GET", "/api/v1/me", None).await;
    assert_eq!(me["permissions"], json!([]));
    let body = Some(json!({ "source_yaml": flow("v") }));
    let (status, _) = call_as(app, &vic, "POST", "/api/v1/workflows", body).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let body = Some(json!({ "name": "Vic's" }));
    let (status, created) = call_as(app, &vic, "POST", "/api/v1/workspaces", body).await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    assert_eq!(created["created_by"], json!(vic_id));
    let (_, me) = call_as(app, &vic, "GET", "/api/v1/me", None).await;
    let roles: Vec<&str> = me["memberships"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["role"].as_str().unwrap())
        .collect();
    assert_eq!(roles.len(), 2, "{me}");
    assert!(roles.contains(&"owner") && roles.contains(&"viewer"));
    // Owning a workspace grants nothing in Main.
    assert_eq!(me["permissions"], json!([]), "{me}");
    let (_, listed) = call_as(app, &vic, "GET", "/api/v1/workspaces", None).await;
    assert!(lists(&listed, &id_of(&created)) && lists(&listed, &MAIN.to_string()));
    // Nobody else sees it.
    let (_, listed) = call_as(app, &bob, "GET", "/api/v1/workspaces", None).await;
    assert!(!lists(&listed, &id_of(&created)));
}

#[tokio::test]
async fn acl_invites_grant_at_most_the_inviters_role_where_they_are() {
    let k = kernel().await;
    let app = &k.app;
    let x = workspace(app, "X").await;
    let (ada, _) = member(&k.store, "ada", &[(MAIN, Role::Admin)]).await;
    let smtp = json!({ "host": "smtp.corp.io", "port": 587, "from": "sun@corp.io",
        "tls": "starttls" });
    let (status, _) = call_as(app, &ada, "PUT", "/api/v1/smtp", Some(smtp)).await;
    assert_eq!(status, StatusCode::OK);
    let invite = |name: &str, extra: Value| {
        let mut body = json!({ "email": format!("{name}@corp.io"), "username": name,
            "password": "a long enough password" });
        for (key, value) in extra.as_object().unwrap() {
            body[key] = value.clone();
        }
        Some(body)
    };
    let post = |token: &str, body: Option<Value>| {
        let token = token.to_owned();
        async move { call_as(app, &token, "POST", "/api/v1/members", body).await }
    };
    // Into Main as member or admin: yes; as owner, or as a deployment
    // owner: no; into X, which ada doesn't see: 404.
    let (status, cy) = post(&ada, invite("cy", json!({ "role": "member" }))).await;
    assert_eq!(status, StatusCode::CREATED, "{cy}");
    let (status, _) = post(&ada, invite("al", json!({ "role": "admin" }))).await;
    assert_eq!(status, StatusCode::CREATED);
    let (status, _) = post(&ada, invite("ow", json!({ "role": "owner" }))).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, _) = post(&ada, invite("o", json!({ "is_owner": true }))).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let into_x = json!({ "role": "viewer", "workspace_id": x });
    let (status, _) = post(&ada, invite("xi", into_x.clone())).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    // The token holder may; the new member sees X only.
    let (status, xi) = post(TOKEN, invite("xi", into_x)).await;
    assert_eq!(status, StatusCode::CREATED, "{xi}");
    let (_, audit) = call_as(app, &ada, "GET", "/api/v1/audit", None).await;
    let audit = audit.to_string();
    assert!(audit.contains("cy@corp.io into Main as member"), "{audit}");
    assert!(audit.contains("xi@corp.io into X as viewer"), "{audit}");

    // A member has none of the member or settings permissions.
    let (bo, _) = member(&k.store, "bo", &[(MAIN, Role::Member)]).await;
    let (status, _) = post(&bo, invite("b2", json!({ "role": "viewer" }))).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    for path in ["/api/v1/members", "/api/v1/audit", "/api/v1/smtp"] {
        let (status, _) = call_as(app, &bo, "GET", path, None).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{path}");
    }
}

/// Things on one workspace.
struct On {
    workspace: String,
    label: &'static str,
    workflow: String,
    task: String,
    source: String,
    run: String,
    /// An `unknown` side effect of `run`, waiting for a human.
    outbox: String,
}

/// Roles × Main / X workspace × actions. A workspace is visible to its
/// members and deployment owners; a hidden one answers 404 everywhere, and
/// lists leave it out.
#[tokio::test]
async fn acl_workspace_visibility_matrix() {
    let k = kernel().await;
    let app = &k.app;
    let owner = |method: &'static str, path: String, body: Option<Value>| async move {
        let (status, value) = call_as(app, TOKEN, method, &path, body).await;
        assert!(status.is_success(), "{method} {path}: {status} {value}");
        value
    };
    let main = MAIN.to_string();
    let x_id = workspace(app, "X").await;
    let y_id = workspace(app, "Y").await;
    let (x, y) = (x_id.to_string(), y_id.to_string());

    // Per workspace: a workflow, a task with its run, a repo and a source.
    let mut on = Vec::new();
    for (workspace, label) in [(&main, "main"), (&x, "x")] {
        let body = json!({ "source_yaml": flow(&format!("w-{label}")), "workspace_id": workspace });
        let workflow = id_of(&owner("POST", "/api/v1/workflows".into(), Some(body)).await);
        let body = json!({ "title": "t", "workflow_id": workflow });
        let task = id_of(&owner("POST", "/api/v1/tasks".into(), Some(body)).await);
        let body = json!({ "url": format!("https://example.com/{label}") });
        let path = format!("/api/v1/workspaces/{workspace}/repos");
        owner("POST", path, Some(body)).await;
        let body =
            json!({ "kind": "upload", "name": format!("s-{label}"), "workspace_id": workspace });
        let source = id_of(&owner("POST", "/api/v1/knowledge/sources".into(), Some(body)).await);
        let mut run = Value::Null;
        for _ in 0..250 {
            run = owner("GET", format!("/api/v1/tasks/{task}"), None).await["run"]["id"].clone();
            if run.is_string() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let run = run.as_str().expect("the task started").to_owned();
        let run_id: RunId = run.parse().unwrap();
        let outbox = OutboxId::generate();
        let mut tx = k.store.begin().await.unwrap();
        tx.enqueue(&OutboxItem {
            id: outbox,
            run_id: Some(run_id),
            idem_key: IdemKey::new(run_id, &StepKey::new("ship").unwrap(), "acl.test"),
            target: "acl.test".into(),
            payload: json!({}),
            status: OutboxStatus::Unknown,
            result: None,
        })
        .await
        .unwrap();
        tx.commit().await.unwrap();
        let outbox = outbox.to_string();
        on.push(On {
            workspace: workspace.clone(),
            label,
            workflow,
            task,
            source,
            run,
            outbox,
        });
    }

    // Who, their roles, and whether they see Main, X and Y.
    let callers = [
        ("u_viewer", vec![(MAIN, Role::Viewer)], [true, false, false]),
        ("u_member", vec![(MAIN, Role::Member)], [true, false, false]),
        ("u_xer", vec![(x_id, Role::Member)], [false, true, false]),
        ("u_admin", vec![(MAIN, Role::Admin)], [true, false, false]),
        (
            "u_everywhere",
            vec![
                (MAIN, Role::Admin),
                (x_id, Role::Admin),
                (y_id, Role::Admin),
            ],
            [true, true, true],
        ),
    ];
    for (name, roles, sees) in &callers {
        let (token, _) = member(&k.store, name, roles).await;
        let me = |method: &'static str, path: String, body: Option<Value>| {
            let token = token.clone();
            async move { call_as(app, &token, method, &path, body).await }
        };
        let permissions = me("GET", "/api/v1/me".into(), None).await.1["permissions"].clone();
        let can = |p: &str| permissions.as_array().unwrap().iter().any(|v| v == p);
        let (_, listed) = me("GET", "/api/v1/workspaces".into(), None).await;
        for (workspace, want) in [&main, &x, &y].into_iter().zip(sees) {
            assert_eq!(
                lists(&listed, workspace),
                *want,
                "{name} lists workspace {workspace}"
            );
        }
        let (_, all_tasks) = me("GET", "/api/v1/tasks".into(), None).await;
        let (_, all_workflows) = me("GET", "/api/v1/workflows".into(), None).await;
        let (_, all_sources) = me("GET", "/api/v1/knowledge/sources".into(), None).await;
        for o in &on {
            let visible = sees[usize::from(o.workspace != main)];
            let what = format!("{name} on {}", o.label);
            let want = |ok: StatusCode, allowed: bool| match (visible, allowed) {
                (false, _) => StatusCode::NOT_FOUND,
                (true, false) => StatusCode::FORBIDDEN,
                (true, true) => ok,
            };
            // Lists without `?workspace=` leave hidden workspaces out.
            assert_eq!(lists(&all_tasks, &o.task), visible, "{what}: tasks");
            assert_eq!(
                lists(&all_workflows, &o.workflow),
                visible,
                "{what}: workflows"
            );
            assert_eq!(lists(&all_sources, &o.source), visible, "{what}: sources");
            let (_, unknown) = me("GET", "/api/v1/outbox?status=unknown".into(), None).await;
            assert_eq!(lists(&unknown, &o.outbox), visible, "{what}: outbox");
            if !visible {
                let resolve = format!("/api/v1/outbox/{}/resolve", o.outbox);
                let body = Some(json!({ "resolution": "delivered" }));
                let (status, _) = me("POST", resolve, body).await;
                assert_eq!(status, StatusCode::NOT_FOUND, "{what}: outbox resolve");
            }
            for path in [
                format!("/api/v1/tasks?workspace={}", o.workspace),
                format!("/api/v1/tasks/{}", o.task),
                format!("/api/v1/workflows?workspace={}", o.workspace),
                format!("/api/v1/workflows/{}", o.workflow),
                format!("/api/v1/workspaces/{}/repos", o.workspace),
                format!("/api/v1/knowledge/sources?workspace={}", o.workspace),
                format!("/api/v1/knowledge/sources/{}/docs", o.source),
                format!("/api/v1/knowledge/search?q=x&workspace={}", o.workspace),
                format!("/api/v1/runs/{}/events", o.run),
            ] {
                let (status, _) = me("GET", path.clone(), None).await;
                assert_eq!(status, want(StatusCode::OK, true), "{what}: GET {path}");
            }
            let rename = Some(json!({ "name": if o.label == "x" { "X" } else { "Main" } }));
            let (status, _) = me(
                "PATCH",
                format!("/api/v1/workspaces/{}", o.workspace),
                rename,
            )
            .await;
            assert_eq!(
                status,
                want(StatusCode::OK, can("edit_workspace")),
                "{what}: rename"
            );
            let yaml = flow(&format!("{name}-{}", o.label));
            let body = Some(json!({ "source_yaml": yaml, "workspace_id": o.workspace }));
            let (status, _) = me("POST", "/api/v1/workflows".into(), body).await;
            let created = want(StatusCode::CREATED, can("create_workflow"));
            assert_eq!(status, created, "{what}: create workflow");
            let body = Some(json!({ "source_yaml": flow(&format!("w-{}", o.label)) }));
            let path = format!("/api/v1/workflows/{}/versions", o.workflow);
            let (status, _) = me("POST", path, body).await;
            let edited = want(StatusCode::CREATED, can("edit_workflow"));
            assert_eq!(status, edited, "{what}: edit workflow");
            let body = Some(json!({ "title": "t", "workflow_id": o.workflow }));
            let (status, _) = me("POST", "/api/v1/tasks".into(), body).await;
            assert_eq!(
                status,
                want(StatusCode::CREATED, can("run")),
                "{what}: new task"
            );
        }
        // A workflow without a workspace goes on Main: only for who sees Main.
        if !sees[0] && can("create_workflow") {
            let body = Some(json!({ "source_yaml": flow(&format!("{name}-default")) }));
            let (status, _) = me("POST", "/api/v1/workflows".into(), body).await;
            assert_eq!(status, StatusCode::NOT_FOUND, "{name}: defaults to Main");
        }
        // Import needs manage_settings and both workspaces: X's source into Main.
        let import = format!(
            "/api/v1/workspaces/{main}/knowledge_sources/{}",
            on[1].source
        );
        let (status, _) = me("POST", import, None).await;
        let want = match (sees[0] && sees[1], can("manage_settings")) {
            (true, true) => StatusCode::NO_CONTENT,
            (false, _) => StatusCode::NOT_FOUND,
            (true, false) => StatusCode::FORBIDDEN,
        };
        assert_eq!(status, want, "{name}: import X's source into Main");
    }
}

/// The resource × verb matrix. A member of Main with each role gets past
/// exactly the routes one of the role's permissions opens (anything but
/// 403; the bodies are invalid, so nothing changes) and a 403 everywhere
/// else. Creating a workspace needs no permission.
#[tokio::test]
async fn acl_permission_matrix_opens_exactly_its_routes() {
    let k = kernel().await;
    let app = &k.app;
    let main = MAIN;
    let probes: Vec<(&str, String, Option<Value>, &[&str])> = vec![
        ("GET", "/members".into(), None, &["view_members"]),
        (
            "POST",
            "/members".into(),
            Some(json!({ "email": "bad", "username": "x" })),
            &["invite_members"],
        ),
        (
            "POST",
            "/members".into(),
            Some(json!({ "email": "bad", "username": "x", "password": "p" })),
            &["create_members"],
        ),
        (
            "PUT",
            "/members/nope/password".into(),
            Some(json!({ "password": "p" })),
            &["edit_members"],
        ),
        (
            "POST",
            "/members/nope/invite".into(),
            None,
            &["edit_members"],
        ),
        (
            "POST",
            "/members/nope/deactivate".into(),
            None,
            &["deactivate_members"],
        ),
        (
            "POST",
            "/members/nope/activate".into(),
            None,
            &["deactivate_members"],
        ),
        ("GET", "/smtp".into(), None, &["manage_settings"]),
        ("GET", "/settings/sso".into(), None, &["manage_settings"]),
        ("GET", "/audit".into(), None, &["manage_settings"]),
        (
            "PUT",
            "/settings/embeddings".into(),
            Some(json!({})),
            &["manage_settings"],
        ),
        (
            "PATCH",
            format!("/workspaces/{main}"),
            Some(json!({ "name": "" })),
            &["edit_workspace"],
        ),
        (
            "DELETE",
            format!("/workspaces/{main}"),
            None,
            &["delete_workspace"],
        ),
        // 2-step resets, the 2-step policy, SMTP and SSO providers.
        (
            "DELETE",
            "/members/nope/two_step".into(),
            None,
            &["edit_members"],
        ),
        (
            "GET",
            "/settings/security".into(),
            None,
            &["manage_settings"],
        ),
        (
            "PUT",
            "/settings/security".into(),
            Some(json!({ "fly": true })),
            &["manage_settings"],
        ),
        (
            "PUT",
            "/smtp".into(),
            Some(json!({ "host": "" })),
            &["manage_settings"],
        ),
        ("POST", "/smtp/test".into(), None, &["manage_settings"]),
        (
            "PUT",
            "/settings/sso/Bad-Id".into(),
            Some(json!({ "kind": "nope" })),
            &["manage_settings"],
        ),
        (
            "DELETE",
            "/settings/sso/none_here".into(),
            None,
            &["manage_settings"],
        ),
    ];
    for role in [Role::Viewer, Role::Member, Role::Admin] {
        let name = format!("p_{role}");
        let (token, _) = member(&k.store, &name, &[(main, role)]).await;
        for (method, path, body, opens) in &probes {
            let path = format!("/api/v1{path}");
            let (status, value) = call_as(app, &token, method, &path, body.clone()).await;
            let want_open = role
                .permissions()
                .iter()
                .any(|p| opens.contains(&p.as_str()));
            assert_eq!(
                status != StatusCode::FORBIDDEN,
                want_open,
                "{role}: {method} {path} -> {status} {value}"
            );
        }
        let body = Some(json!({ "name": "" }));
        let (status, _) = call_as(app, &token, "POST", "/api/v1/workspaces", body).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{role}: create");
    }
}

/// The workspace verbs don't leak a hidden workspace: an admin of Main gets
/// 404 (not 403) on X's delete and rename, and X stays.
#[tokio::test]
async fn acl_workspace_verbs_answer_404_on_a_hidden_workspace() {
    let k = kernel().await;
    let app = &k.app;
    let x = workspace(app, "X").await;
    let (token, _) = member(&k.store, "crew", &[(MAIN, Role::Admin)]).await;
    let x = format!("/api/v1/workspaces/{x}");
    let probes = [
        ("DELETE", x.clone(), None),
        ("PATCH", x.clone(), Some(json!({ "name": "Mine now" }))),
    ];
    for (method, path, body) in probes {
        let (status, value) = call_as(app, &token, method, &path, body).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{method} {path}: {value}");
    }
    let (_, workspaces) = call_as(app, TOKEN, "GET", "/api/v1/workspaces", None).await;
    assert!(
        workspaces
            .as_array()
            .unwrap()
            .iter()
            .any(|b| b["name"] == "X"),
        "X stays: {workspaces}"
    );
    let (_, theirs) = call_as(app, &token, "GET", "/api/v1/workspaces", None).await;
    assert!(
        !theirs.to_string().contains("\"X\""),
        "and stays hidden: {theirs}"
    );
}

/// Every signed-in member, even one with no permission at all,
/// manages their own second steps (no 403 on any `/me/two_step` route).
#[tokio::test]
async fn acl_own_two_step_routes_need_no_permission() {
    let k = kernel().await;
    let app = &k.app;
    let (token, _) = member(&k.store, "nobody", &[]).await;
    let probes = [
        ("GET", "/api/v1/me/two_step", None, StatusCode::OK),
        (
            "POST",
            "/api/v1/me/two_step/totp",
            Some(json!({})),
            StatusCode::OK,
        ),
        (
            "POST",
            "/api/v1/me/two_step/totp/confirm",
            Some(json!({ "id": "nope", "code": "000000" })),
            StatusCode::UNPROCESSABLE_ENTITY,
        ),
        // No SMTP yet.
        (
            "POST",
            "/api/v1/me/two_step/email",
            None,
            StatusCode::CONFLICT,
        ),
        // The test kernel is at http://localhost: passkeys work.
        ("POST", "/api/v1/me/two_step/passkey", None, StatusCode::OK),
        (
            "POST",
            "/api/v1/me/two_step/challenge",
            None,
            StatusCode::UNPROCESSABLE_ENTITY,
        ),
    ];
    for (method, path, body, want) in probes {
        let (status, value) = call_as(app, &token, method, path, body).await;
        assert_eq!(status, want, "{method} {path}: {value}");
    }
    let unknown = format!("/api/v1/me/two_step/{}", sun_core::FactorId::generate());
    let (status, _) = call_as(app, &token, "DELETE", &unknown, None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

/// Manual chat links are gone (T15): every former `/chat_links` route is a
/// JSON 404 even for the token owner and an admin, and writes nothing.
#[tokio::test]
async fn acl_chat_link_routes_are_gone_for_admins() {
    let k = kernel().await;
    let app = &k.app;
    let (admin, admin_id) = member(&k.store, "root", &[(MAIN, Role::Admin)]).await;
    let link = json!({ "member_id": admin_id });
    let probes = [
        ("GET", "/api/v1/chat_links", None),
        ("GET", "/api/v1/chat_links/slack/U1", None),
        ("PUT", "/api/v1/chat_links/slack/U1", Some(link)),
        ("DELETE", "/api/v1/chat_links/slack/U1", None),
    ];
    for token in [TOKEN, admin.as_str()] {
        for (method, path, body) in probes.clone() {
            let (status, value) = call_as(app, token, method, path, body).await;
            assert!(
                matches!(
                    status,
                    StatusCode::NOT_FOUND | StatusCode::METHOD_NOT_ALLOWED
                ),
                "{method} {path} -> {status} {value}"
            );
            assert_eq!(
                value["error"]["code"], "not_found",
                "{method} {path}: {value}"
            );
        }
    }
    let mut tx = k.store.read().await.unwrap();
    assert_eq!(
        sun_store::SettingRepo::setting(&mut tx, "chat_links")
            .await
            .unwrap(),
        None
    );
}

/// `caller` sets member `target`'s password: the status.
async fn set_password(app: &Router, caller: &str, target: &str) -> StatusCode {
    let path = format!("/api/v1/members/{target}/password");
    let body = json!({ "password": "a password set by someone else", "must_change": false });
    call_as(app, caller, "PUT", &path, Some(body)).await.0
}

/// The take-over guard covers every route that acts on another member's
/// account (reset code, deactivate, activate, password, 2-step): a member
/// who can do more or sees a workspace the caller can't is out of reach;
/// an equal peer, a weaker member and someone in no workspace are in
/// reach. Deployment owners reach anyone; only they reach an owner.
#[tokio::test]
async fn acl_take_over_guard_reaches_only_members_who_do_and_see_no_more() {
    let k = kernel().await;
    let app = &k.app;
    let x = workspace(app, "X").await;
    let (ed, _) = member(&k.store, "ed", &[(MAIN, Role::Admin)]).await;
    let (_, peer) = member(&k.store, "peer", &[(MAIN, Role::Admin)]).await;
    let (_, wider) = member(&k.store, "wider", &[(MAIN, Role::Admin), (x, Role::Viewer)]).await;
    let (_, sam) = member(&k.store, "sam", &[(x, Role::Member)]).await;
    let (_, nobody) = member(&k.store, "nobody", &[]).await;
    let (_, tim) = member(&k.store, "tim", &[(MAIN, Role::Viewer)]).await;
    let (olive, olive_id) = member(&k.store, "olive", &[]).await;
    let (_, oscar) = member(&k.store, "oscar", &[]).await;
    for id in [&olive_id, &oscar] {
        let path = format!("/api/v1/members/{id}/owner");
        let body = Some(json!({ "is_owner": true }));
        let (status, value) = call_as(app, TOKEN, "PUT", &path, body).await;
        assert_eq!(status, StatusCode::OK, "{value}");
    }
    for (caller, target, want, what) in [
        (&ed, &peer, StatusCode::OK, "equal peer"),
        (&ed, &nobody, StatusCode::OK, "in no workspace"),
        (&ed, &wider, StatusCode::FORBIDDEN, "peer who also sees X"),
        (&ed, &sam, StatusCode::FORBIDDEN, "on X only"),
        (&ed, &oscar, StatusCode::FORBIDDEN, "a deployment owner"),
        (&olive, &wider, StatusCode::OK, "owner on X's member"),
        (&olive, &sam, StatusCode::OK, "owner on X's member"),
        (&olive, &oscar, StatusCode::OK, "owner on owner"),
    ] {
        assert_eq!(set_password(app, caller, target).await, want, "{what}");
    }
    let probes = |id: &str| {
        let m = format!("/api/v1/members/{id}");
        let password = json!({ "password": "the editor knows this one" });
        [
            ("POST", format!("{m}/invite"), None),
            ("POST", format!("{m}/deactivate"), None),
            ("POST", format!("{m}/activate"), None),
            ("PUT", format!("{m}/password"), Some(password)),
            ("DELETE", format!("{m}/two_step"), None),
        ]
    };
    for id in [&wider, &sam] {
        for (method, path, body) in probes(id) {
            let (status, value) = call_as(app, &ed, method, &path, body).await;
            assert_eq!(status, StatusCode::FORBIDDEN, "{method} {path}: {value}");
        }
    }
    for (method, path, body) in probes(&tim) {
        if path.ends_with("/invite") {
            continue; // no SMTP in the test kernel
        }
        let (status, value) = call_as(app, &ed, method, &path, body).await;
        assert!(status.is_success(), "{method} {path}: {status} {value}");
    }
}
