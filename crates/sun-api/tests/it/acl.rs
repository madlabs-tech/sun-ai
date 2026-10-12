//! Groups through the real kernel: admins create, rename and delete
//! groups and pick their permissions; a member has the union of their
//! groups'; `who: role:<key>` survives a rename; every change is audited.
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
use sun_core::model::{OutboxItem, OutboxStatus};
use sun_core::{GroupId, IdemKey, OutboxId, RunId, StepKey};
use sun_store::{GroupRepo, LoginRepo, MemberRepo, OutboxRepo};

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

/// A session token of a new member in `groups`, and their id.
async fn member(store: &sun_store::Store, name: &str, groups: &[GroupId]) -> (String, String) {
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
    tx.set_member_groups(member.id, groups).await.unwrap();
    let expires = chrono::Utc::now() + chrono::Duration::hours(1);
    tx.insert_login_session(&hash, member.id, expires)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    (token, member.id.to_string())
}

const FLOW: &str = "name: ship\nsteps:\n  - id: ok\n    type: approval\n    who: [role:builders]\n";

#[tokio::test]
async fn acl_groups_grant_workspace_and_workflow_permissions() {
    let k = kernel().await;
    let app = &k.app;
    let owner = |method, path, body| call_as(app, TOKEN, method, path, body);

    // After install a plain Member can't touch workspaces, but creates and edits
    // workflows; `run` alone no longer edits workflows.
    let (bob, bob_id) = member(&k.store, "bob", &[GroupId::VIEWER, GroupId::MEMBER]).await;
    let (status, me) = call_as(app, &bob, "GET", "/api/v1/me", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        me["permissions"],
        json!(["run", "approve", "create_workflow", "edit_workflow"])
    );
    let groups: Vec<&str> = me["groups"]
        .as_array()
        .unwrap()
        .iter()
        .map(|g| g["name"].as_str().unwrap())
        .collect();
    assert_eq!(groups, ["Viewer", "User"]);
    let workspace = Some(json!({ "name": "X" }));
    let (status, _) = call_as(app, &bob, "POST", "/api/v1/workspaces", workspace.clone()).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, _) = call_as(app, &bob, "GET", "/api/v1/groups", None).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "groups need view_members");

    let (runner, rex_id) = member(&k.store, "rex", &[]).await;
    let (status, runners) = owner(
        "POST",
        "/api/v1/groups",
        Some(json!({ "name": "Runners", "permissions": ["run"] })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let path = format!("/api/v1/groups/{}/members", runners["id"].as_str().unwrap());
    let (status, listed) = owner("PUT", &path, Some(json!({ "member_ids": [] }))).await;
    assert_eq!((status, listed), (StatusCode::OK, json!([])));
    let rex_groups = format!("/api/v1/members/{rex_id}/groups");
    let (status, _) = owner(
        "PUT",
        &rex_groups,
        // Viewer: Main is open to it, so the 403 is about `run`.
        Some(json!({ "group_ids": [GroupId::VIEWER, runners["id"]] })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let flow = Some(json!({ "source_yaml": FLOW }));
    let (status, _) = call_as(app, &runner, "POST", "/api/v1/workflows", flow.clone()).await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "run doesn't create workflows"
    );
    let (status, _) = call_as(
        app,
        &runner,
        "POST",
        "/api/v1/workflows/validate",
        Some(json!({ "source_yaml": FLOW })),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    // The owner creates group B with the workspace and workflow permissions,
    // and puts bob in it.
    let b = json!({ "name": "Builders", "permissions": ["create_workspace", "edit_workspace",
        "create_workflow", "edit_workflow", "access_all_workspaces"] });
    let (status, group) = owner("POST", "/api/v1/groups", Some(b.clone())).await;
    assert_eq!(status, StatusCode::CREATED, "{group}");
    assert_eq!(group["key"], "builders");
    let (status, _) = owner("POST", "/api/v1/groups", Some(b)).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "names are unique");
    let group_path = format!("/api/v1/groups/{}", group["id"].as_str().unwrap());
    let members_path = format!("{group_path}/members");
    let (status, members) = owner(
        "PUT",
        &members_path,
        Some(json!({ "member_ids": [bob_id] })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(members[0]["username"], "bob");

    // Bob now creates and renames a workspace, and creates and edits a workflow
    // on it.
    let (status, x) = call_as(app, &bob, "POST", "/api/v1/workspaces", workspace).await;
    assert_eq!(status, StatusCode::CREATED, "{x}");
    let x_path = format!("/api/v1/workspaces/{}", x["id"].as_str().unwrap());
    let rename = Some(json!({ "name": "X2" }));
    let (status, _) = call_as(app, &bob, "PATCH", &x_path, rename).await;
    assert_eq!(status, StatusCode::OK);
    let on_x = Some(json!({ "source_yaml": FLOW, "workspace_id": x["id"] }));
    let (status, workflow) = call_as(app, &bob, "POST", "/api/v1/workflows", on_x).await;
    assert_eq!(status, StatusCode::CREATED, "{workflow}");
    let versions = format!(
        "/api/v1/workflows/{}/versions",
        workflow["id"].as_str().unwrap()
    );
    let edited = Some(json!({ "source_yaml": FLOW }));
    let (status, _) = call_as(app, &bob, "POST", &versions, edited.clone()).await;
    assert_eq!(status, StatusCode::CREATED);
    // X has no groups (admins and access_all_workspaces only): rex doesn't
    // even see the workflow.
    let (status, _) = call_as(app, &runner, "POST", &versions, edited).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "X is hidden from rex");

    // Renaming keeps the key: `who: [role:builders]` still admits bob.
    let (status, renamed) = owner("PATCH", &group_path, Some(json!({ "name": "Group B" }))).await;
    assert_eq!(
        (status, renamed["key"].as_str(), renamed["name"].as_str()),
        (StatusCode::OK, Some("builders"), Some("Group B"))
    );
    let task = json!({ "title": "t", "workflow_id": workflow["id"] });
    let (status, task) = call_as(app, &bob, "POST", "/api/v1/tasks", Some(task)).await;
    assert_eq!(status, StatusCode::CREATED, "{task}");
    let task_path = format!("/api/v1/tasks/{}", task["id"].as_str().unwrap());
    for _ in 0..200 {
        let (_, view) = call_as(app, &bob, "GET", &task_path, None).await;
        if view["status"] == "waiting_approval" {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let approve = format!("{task_path}/approve");
    let (status, view) = call_as(app, &bob, "POST", &approve, Some(json!({}))).await;
    assert_eq!(status, StatusCode::OK, "{view}");

    // Deleting the group takes its permissions away at once, and with
    // access_all_workspaces gone, X (no groups) is hidden from bob.
    let (status, _) = owner("DELETE", &group_path, None).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (status, _) = call_as(app, &bob, "PATCH", &x_path, Some(json!({ "name": "Y" }))).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let main_path = format!("/api/v1/workspaces/{}", sun_core::WorkspaceId::MAIN);
    let (status, _) = call_as(app, &bob, "PATCH", &main_path, Some(json!({ "name": "M" }))).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "edit_workspace is gone too");
    let (status, _) = owner("DELETE", &group_path, None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // Every group change is audited.
    let (_, audit) = owner("GET", "/api/v1/audit", None).await;
    let details: Vec<String> = audit
        .as_array()
        .unwrap()
        .iter()
        .filter(|e| e["action"] == "group_change")
        .map(|e| {
            format!(
                "{} {}",
                e["target"].as_str().unwrap(),
                e["detail"].as_str().unwrap()
            )
        })
        .collect();
    for want in [
        "Builders created as role:builders",
        "Group B renamed from Builders",
        "Builders members: bob",
        "Group B deleted (was role:builders)",
    ] {
        assert!(
            details.iter().any(|d| d.starts_with(want)),
            "{want}: {details:?}"
        );
    }
}

#[tokio::test]
async fn acl_invites_pick_groups_and_admins_manage_settings() {
    let k = kernel().await;
    let app = &k.app;
    let (ada, _) = member(&k.store, "ada", &[GroupId::ADMIN]).await;
    // SMTP first, so invites can go out.
    let smtp = json!({ "host": "smtp.corp.io", "port": 587, "from": "sun@corp.io",
        "tls": "starttls" });
    let (status, _) = call_as(app, &ada, "PUT", "/api/v1/smtp", Some(smtp)).await;
    assert_eq!(status, StatusCode::OK);

    let (_, groups) = call_as(app, &ada, "GET", "/api/v1/groups", None).await;
    let keys: Vec<&str> = groups
        .as_array()
        .unwrap()
        .iter()
        .map(|g| g["key"].as_str().unwrap())
        .collect();
    assert_eq!(keys, ["viewer", "member", "admin"]);
    let invite = json!({ "email": "cy@corp.io", "username": "cy",
        "group_ids": [GroupId::MEMBER] });
    let (status, cy) = call_as(app, &ada, "POST", "/api/v1/members", Some(invite)).await;
    assert_eq!(status, StatusCode::CREATED, "{cy}");
    assert_eq!(
        (&cy["groups"], &cy["is_owner"]),
        (&json!([GroupId::MEMBER]), &json!(false))
    );
    // Only an owner invites an owner; unknown groups are 404.
    let owner = json!({ "email": "o@corp.io", "username": "o", "is_owner": true });
    let (status, _) = call_as(app, &ada, "POST", "/api/v1/members", Some(owner)).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let stray = json!({ "email": "s@corp.io", "username": "s",
        "group_ids": [GroupId::generate()] });
    let (status, _) = call_as(app, &ada, "POST", "/api/v1/members", Some(stray)).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (_, audit) = call_as(app, &ada, "GET", "/api/v1/audit", None).await;
    assert!(
        audit.to_string().contains("invited cy@corp.io into [User]"),
        "{audit}"
    );

    // An admin (manage_settings) also imports across workspaces; a group without
    // it can't, even with every workspace permission.
    let mut tx = k.store.begin().await.unwrap();
    let all_workspaces = tx
        .insert_group(
            "Workspaces",
            &[
                sun_core::model::Permission::CreateWorkspace,
                sun_core::model::Permission::EditWorkspace,
            ],
        )
        .await
        .unwrap();
    tx.commit().await.unwrap();
    let (bo, _) = member(&k.store, "bo", &[all_workspaces.id]).await;
    let (_, x) = call_as(
        app,
        &bo,
        "POST",
        "/api/v1/workspaces",
        Some(json!({ "name": "X", "group_ids": [all_workspaces.id] })),
    )
    .await;
    let import = format!(
        "/api/v1/workspaces/{}/knowledge_sources/nothing",
        x["id"].as_str().unwrap()
    );
    let (status, _) = call_as(app, &bo, "POST", &import, None).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    for path in ["/api/v1/members", "/api/v1/audit", "/api/v1/smtp"] {
        let (status, _) = call_as(app, &bo, "GET", path, None).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{path}");
    }
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

/// Groups × open (Main) / restricted (X) workspace × actions. A workspace is
/// visible to its groups, owners and `access_all_workspaces`; a hidden one
/// answers 404 everywhere, and lists leave it out.
#[tokio::test]
async fn acl_workspace_visibility_matrix() {
    let k = kernel().await;
    let app = &k.app;
    let owner = |method: &'static str, path: String, body: Option<Value>| async move {
        let (status, value) = call_as(app, TOKEN, method, &path, body).await;
        assert!(status.is_success(), "{method} {path}: {status} {value}");
        value
    };
    let workspaces = owner("GET", "/api/v1/workspaces".into(), None).await;
    let main = id_of(&workspaces[0]);
    assert_eq!(
        workspaces[0]["group_ids"],
        json!([GroupId::VIEWER, GroupId::MEMBER, GroupId::ADMIN]),
        "the upgrade keeps Main open to everyone"
    );
    let group = |name: &'static str, permissions: Value| {
        owner(
            "POST",
            "/api/v1/groups".into(),
            Some(json!({ "name": name, "permissions": permissions })),
        )
    };
    let xers = id_of(&group("Xers", json!(["run", "create_workflow", "edit_workflow"])).await);
    let builders = id_of(
        &group(
            "Builders",
            json!([
                "create_workspace",
                "edit_workspace",
                "create_workflow",
                "edit_workflow"
            ]),
        )
        .await,
    );
    let keepers = id_of(&group("Keepers", json!(["manage_settings"])).await);
    let new_workspace = |body: Value| owner("POST", "/api/v1/workspaces".into(), Some(body));
    let x = id_of(&new_workspace(json!({ "name": "X", "group_ids": [xers] })).await);
    let y = id_of(&new_workspace(json!({ "name": "Y" })).await);

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

    // Who, their groups, and whether they see Main, X and Y.
    let (viewer, member_group, admin) = (GroupId::VIEWER, GroupId::MEMBER, GroupId::ADMIN);
    let id = |g: &String| g.parse::<GroupId>().unwrap();
    let callers = [
        ("u_viewer", vec![viewer], [true, false, false]),
        ("u_member", vec![viewer, member_group], [true, false, false]),
        ("u_xer", vec![id(&xers)], [false, true, false]),
        (
            "u_builder",
            vec![member_group, id(&builders)],
            [true, false, false],
        ),
        ("u_keeper", vec![viewer, id(&keepers)], [true, false, false]),
        (
            "u_admin",
            vec![viewer, member_group, admin],
            [true, true, true],
        ),
    ];
    for (name, groups, sees) in &callers {
        let (token, _) = member(&k.store, name, groups).await;
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

    // Workspace access: creating a workspace and `PUT /workspaces/{id}/groups` give or
    // take only the caller's own groups without manage_groups.
    let (builder, _) = member(&k.store, "u_bo", &[id(&builders)]).await;
    let new = |name: &str, groups: Value| Some(json!({ "name": name, "group_ids": groups }));
    let (status, _) = call_as(
        app,
        &builder,
        "POST",
        "/api/v1/workspaces",
        new("B1", json!([admin])),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    let body = new("B2", json!([builders]));
    let (status, b) = call_as(app, &builder, "POST", "/api/v1/workspaces", body).await;
    assert_eq!(
        (status, &b["group_ids"]),
        (StatusCode::CREATED, &json!([builders]))
    );
    let access = format!("/api/v1/workspaces/{}/groups", id_of(&b));
    let both = Some(json!({ "group_ids": [builders, xers] }));
    let (status, _) = call_as(app, &builder, "PUT", &access, both.clone()).await;
    assert_eq!(
        status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "Xers isn't theirs"
    );
    let b = owner("PUT", access.clone(), both.clone()).await;
    // Ordered by group id; ids made in the same millisecond sort either way.
    let mut want = [xers.clone(), builders.clone()];
    want.sort();
    assert_eq!(b["group_ids"], json!(want), "ordered by group");
    // Taking their own group off hides the workspace from them at once.
    let only_xers = Some(json!({ "group_ids": [xers] }));
    let (status, _) = call_as(app, &builder, "PUT", &access, only_xers).await;
    assert_eq!(status, StatusCode::OK);
    let (status, _) = call_as(app, &builder, "PUT", &access, both).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let audit = owner("GET", "/api/v1/audit".into(), None).await.to_string();
    assert!(audit.contains("access: Builders, Xers -> Xers"), "{audit}");
}

/// The resource × verb matrix. A member in a group with exactly one
/// permission gets past exactly that permission's routes (anything but 403;
/// the bodies are invalid, so nothing changes) and a 403 everywhere else.
#[tokio::test]
async fn acl_permission_matrix_opens_exactly_its_routes() {
    let k = kernel().await;
    let app = &k.app;
    let main = sun_core::WorkspaceId::MAIN;
    let (members, groups) = (
        &["view_members", "manage_groups"][..],
        &["manage_groups"][..],
    );
    let probes: Vec<(&str, String, Option<Value>, &[&str])> = vec![
        ("GET", "/members".into(), None, members),
        ("GET", "/groups".into(), None, members),
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
            "PUT",
            "/members/nope/groups".into(),
            Some(json!({ "group_ids": [] })),
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
        (
            "POST",
            "/groups".into(),
            Some(json!({ "name": "" })),
            groups,
        ),
        ("PATCH", "/groups/nope".into(), Some(json!({})), groups),
        ("DELETE", "/groups/nope".into(), None, groups),
        (
            "PUT",
            "/groups/nope/members".into(),
            Some(json!({ "member_ids": [] })),
            groups,
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
            "POST",
            "/workspaces".into(),
            Some(json!({ "name": "" })),
            &["create_workspace"],
        ),
        (
            "PATCH",
            format!("/workspaces/{main}"),
            Some(json!({ "name": "" })),
            &["edit_workspace"],
        ),
        (
            "PUT",
            format!("/workspaces/{main}/groups"),
            Some(json!({})),
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
    let permissions = [
        "none",
        "invite_members",
        "create_members",
        "view_members",
        "edit_members",
        "deactivate_members",
        "manage_groups",
        "manage_settings",
        "create_workspace",
        "edit_workspace",
        "delete_workspace",
    ];
    for permission in permissions {
        let mut tx = k.store.begin().await.unwrap();
        let granted: Vec<sun_core::model::Permission> = permission.parse().into_iter().collect();
        let group = tx.insert_group(permission, &granted).await.unwrap();
        tx.commit().await.unwrap();
        // Viewer (no permissions) keeps Main visible, so a workspace route
        // answers 403, not 404.
        let name = format!("p_{permission}");
        let (token, _) = member(&k.store, &name, &[GroupId::VIEWER, group.id]).await;
        for (method, path, body, opens) in &probes {
            let path = format!("/api/v1{path}");
            let (status, value) = call_as(app, &token, method, &path, body.clone()).await;
            let want_open = opens.contains(&permission);
            assert_eq!(
                status != StatusCode::FORBIDDEN,
                want_open,
                "{permission}: {method} {path} -> {status} {value}"
            );
        }
    }
}

/// No escalation through groups. Without `manage_groups`, a caller
/// adds people to (or takes them out of) only groups whose permissions they
/// hold, and never changes their own groups; owners and `manage_groups` may.
#[tokio::test]
async fn acl_member_permissions_grant_only_what_the_caller_holds() {
    let k = kernel().await;
    let app = &k.app;
    let (viewer, user, admin) = (GroupId::VIEWER, GroupId::MEMBER, GroupId::ADMIN);
    let group = |name: &'static str, permissions: Value| async move {
        let body = json!({ "name": name, "permissions": permissions });
        let (status, g) = call_as(app, TOKEN, "POST", "/api/v1/groups", Some(body)).await;
        assert_eq!(status, StatusCode::CREATED, "{g}");
        g["id"].as_str().unwrap().parse::<GroupId>().unwrap()
    };
    let editors = group(
        "Editors",
        json!(["edit_members", "view_members", "run", "approve"]),
    )
    .await;
    let runners = group("Runners", json!(["run"])).await;
    let inviters = group("Inviters", json!(["invite_members"])).await;
    let creators = group("Creators", json!(["create_members"])).await;
    let keepers = group(
        "Keepers",
        json!(["manage_groups", "create_members", "edit_members"]),
    )
    .await;
    let (editor, editor_id) = member(&k.store, "ed", &[viewer, editors]).await;
    let (_, tim) = member(&k.store, "tim", &[viewer]).await;
    let regroup = |token: &str, id: &str, groups: Value| {
        let (token, path) = (token.to_owned(), format!("/api/v1/members/{id}/groups"));
        async move {
            let body = Some(json!({ "group_ids": groups }));
            call_as(app, &token, "PUT", &path, body).await.0
        }
    };
    let forbidden = StatusCode::FORBIDDEN;

    // edit_members: not into Admin, nor User (create_workflow, which they
    // lack), nor their own groups; between groups they hold, yes.
    assert_eq!(regroup(&editor, &tim, json!([admin])).await, forbidden);
    assert_eq!(
        regroup(&editor, &tim, json!([viewer, user])).await,
        forbidden
    );
    let own = json!([viewer, editors, admin]);
    assert_eq!(regroup(&editor, &editor_id, own).await, forbidden);
    let own = json!([viewer, editors, runners]);
    assert_eq!(regroup(&editor, &editor_id, own).await, forbidden);
    let ok = StatusCode::OK;
    assert_eq!(regroup(&editor, &tim, json!([viewer, runners])).await, ok);
    assert_eq!(regroup(&editor, &tim, json!([runners])).await, ok);
    // Nor out of Admin.
    assert_eq!(regroup(TOKEN, &tim, json!([admin])).await, ok);
    assert_eq!(regroup(&editor, &tim, json!([viewer])).await, forbidden);

    // invite_members / create_members: not into Admin.
    let invite = |name: &str, groups: Value, password: bool| {
        let mut body = json!({ "email": format!("{name}@corp.io"), "username": name,
            "group_ids": groups });
        if password {
            body["password"] = json!("a long enough password");
        }
        Some(body)
    };
    let (inviter, _) = member(&k.store, "iv", &[viewer, inviters]).await;
    let (creator, _) = member(&k.store, "cr", &[viewer, creators]).await;
    let (keeper, keeper_id) = member(&k.store, "kp", &[keepers]).await;
    let post = |token: &str, body: Option<Value>| {
        let token = token.to_owned();
        async move {
            call_as(app, &token, "POST", "/api/v1/members", body)
                .await
                .0
        }
    };
    let into_admin = invite("a1", json!([admin]), false);
    assert_eq!(post(&inviter, into_admin).await, forbidden);
    // Viewer grants no permission and only Main, which they see: on to
    // the SMTP check (409, none set up).
    let into_viewer = invite("a2", json!([viewer]), false);
    assert_eq!(post(&inviter, into_viewer).await, StatusCode::CONFLICT);
    let into_admin = invite("a3", json!([admin]), true);
    assert_eq!(post(&creator, into_admin).await, forbidden);
    let into_viewer = invite("a4", json!([viewer]), true);
    assert_eq!(post(&creator, into_viewer).await, StatusCode::CREATED);

    // manage_groups and owners grant anything, their own groups too.
    let into_admin = invite("a5", json!([admin]), true);
    assert_eq!(post(&keeper, into_admin).await, StatusCode::CREATED);
    let into_admin = invite("a6", json!([admin]), true);
    assert_eq!(post(TOKEN, into_admin).await, StatusCode::CREATED);
    assert_eq!(regroup(&keeper, &tim, json!([viewer])).await, ok);
    let own = json!([keepers, admin]);
    assert_eq!(regroup(&keeper, &keeper_id, own).await, ok);
}

/// The new workspace verbs don't leak a hidden workspace. A group with
/// every workspace permission but not attached to workspace X gets 404 (not 403)
/// on X's delete, rename and access routes, and X stays.
#[tokio::test]
async fn acl_workspace_verbs_answer_404_on_a_hidden_workspace() {
    let k = kernel().await;
    let app = &k.app;
    let body = json!({ "name": "Workspace crew",
        "permissions": ["create_workspace", "edit_workspace", "delete_workspace"] });
    let (_, crew) = call_as(app, TOKEN, "POST", "/api/v1/groups", Some(body)).await;
    let body = json!({ "name": "Secret team", "permissions": [] });
    let (_, secret) = call_as(app, TOKEN, "POST", "/api/v1/groups", Some(body)).await;
    let body = json!({ "name": "X", "group_ids": [secret["id"]] });
    let (status, x) = call_as(app, TOKEN, "POST", "/api/v1/workspaces", Some(body)).await;
    assert_eq!(status, StatusCode::CREATED, "{x}");
    let crew_id: GroupId = id_of(&crew).parse().unwrap();
    let (token, _) = member(&k.store, "crew", &[GroupId::VIEWER, crew_id]).await;
    let x = format!("/api/v1/workspaces/{}", id_of(&x));
    let probes = [
        ("DELETE", x.clone(), None),
        ("PATCH", x.clone(), Some(json!({ "name": "Mine now" }))),
        (
            "PUT",
            format!("{x}/groups"),
            Some(json!({ "group_ids": [crew_id] })),
        ),
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
    let (admin, admin_id) = member(
        &k.store,
        "root",
        &[GroupId::VIEWER, GroupId::MEMBER, GroupId::ADMIN],
    )
    .await;
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

/// A group that holds no permission still opens the workspaces it is attached
/// to. Putting someone in it hands them those workspaces, so the escalation
/// guard should count it as a grant the caller can't make when they can't
/// see the workspace.
#[tokio::test]
async fn acl_granting_a_group_never_opens_a_workspace_the_caller_cannot_see() {
    let k = kernel().await;
    let app = &k.app;
    let body = json!({ "name": "Secret team", "permissions": [] });
    let (_, secret) = call_as(app, TOKEN, "POST", "/api/v1/groups", Some(body)).await;
    let body = json!({ "name": "X", "group_ids": [secret["id"]] });
    let (_, x) = call_as(app, TOKEN, "POST", "/api/v1/workspaces", Some(body)).await;
    let body = json!({ "name": "Recruiters", "permissions": ["create_members", "edit_members"] });
    let (_, recruiters) = call_as(app, TOKEN, "POST", "/api/v1/groups", Some(body)).await;
    let recruiters: GroupId = id_of(&recruiters).parse().unwrap();
    let (token, _) = member(&k.store, "rex", &[GroupId::VIEWER, recruiters]).await;
    let x = format!("/api/v1/workspaces/{}", id_of(&x));
    let (status, _) = call_as(app, &token, "GET", &x, None).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "rex can't see X");

    // A second account of rex's own, in the group that sees X.
    let mole = json!({ "email": "mole@corp.io", "username": "mole",
        "group_ids": [secret["id"]], "password": "a long enough password", "must_change": false });
    let (status, _) = call_as(app, &token, "POST", "/api/v1/members", Some(mole)).await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "a new account into Secret team"
    );
    // Nor moving someone else in.
    let (_, tim) = member(&k.store, "tim", &[GroupId::VIEWER]).await;
    let regroup = format!("/api/v1/members/{tim}/groups");
    let body = json!({ "group_ids": [GroupId::VIEWER, secret["id"]] });
    let (status, _) = call_as(app, &token, "PUT", &regroup, Some(body)).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "tim into Secret team");
}

/// `edit_members` without `manage_groups` can't put anyone in Admin, so it
/// must not become an Admin another way: by setting a (non-owner) Admin's
/// password and clearing their 2-step.
#[tokio::test]
async fn acl_edit_members_cannot_take_over_a_stronger_member() {
    let k = kernel().await;
    let app = &k.app;
    let body = json!({ "name": "Editors", "permissions": ["view_members", "edit_members"] });
    let (_, editors) = call_as(app, TOKEN, "POST", "/api/v1/groups", Some(body)).await;
    let editors: GroupId = id_of(&editors).parse().unwrap();
    let (editor, _) = member(&k.store, "ed", &[GroupId::VIEWER, editors]).await;
    let (_, ada) = member(&k.store, "ada", &[GroupId::ADMIN]).await;

    let password = json!({ "password": "the editor knows this one", "must_change": false });
    let path = format!("/api/v1/members/{ada}/password");
    let (status, _) = call_as(app, &editor, "PUT", &path, Some(password)).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "set an Admin's password");
    let path = format!("/api/v1/members/{ada}/two_step");
    let (status, _) = call_as(app, &editor, "DELETE", &path, None).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "reset an Admin's 2-step");
}

/// B4f1: the take-over guard covers every route that acts on another
/// member's account. Without `manage_groups`, a member who can do more
/// (Admin) or sees a workspace the caller can't (Secret team on X) is out of
/// reach: reset code, deactivate, activate, groups, password and 2-step.
/// A weaker member stays in reach.
#[tokio::test]
async fn acl_member_actions_reach_only_weaker_members() {
    let k = kernel().await;
    let app = &k.app;
    let body = json!({ "name": "Editors",
        "permissions": ["view_members", "edit_members", "deactivate_members"] });
    let (_, editors) = call_as(app, TOKEN, "POST", "/api/v1/groups", Some(body)).await;
    let editors: GroupId = id_of(&editors).parse().unwrap();
    let body = json!({ "name": "Secret team", "permissions": [] });
    let (_, secret) = call_as(app, TOKEN, "POST", "/api/v1/groups", Some(body)).await;
    let secret: GroupId = id_of(&secret).parse().unwrap();
    let body = json!({ "name": "X", "group_ids": [secret] });
    let (status, x) = call_as(app, TOKEN, "POST", "/api/v1/workspaces", Some(body)).await;
    assert_eq!(status, StatusCode::CREATED, "{x}");
    let (editor, _) = member(&k.store, "ed", &[GroupId::VIEWER, editors]).await;
    let (_, ada) = member(&k.store, "ada", &[GroupId::ADMIN]).await;
    let (_, sam) = member(&k.store, "sam", &[secret]).await;
    let (_, tim) = member(&k.store, "tim", &[GroupId::VIEWER]).await;
    let probes = |id: &str, groups: Value| {
        let m = format!("/api/v1/members/{id}");
        let password = json!({ "password": "the editor knows this one" });
        [
            ("POST", format!("{m}/invite"), None),
            ("POST", format!("{m}/deactivate"), None),
            ("POST", format!("{m}/activate"), None),
            (
                "PUT",
                format!("{m}/groups"),
                Some(json!({ "group_ids": groups })),
            ),
            ("PUT", format!("{m}/password"), Some(password)),
            ("DELETE", format!("{m}/two_step"), None),
        ]
    };
    for (id, groups) in [(&ada, json!([GroupId::ADMIN])), (&sam, json!([secret]))] {
        for (method, path, body) in probes(id, groups) {
            let (status, value) = call_as(app, &editor, method, &path, body).await;
            assert_eq!(status, StatusCode::FORBIDDEN, "{method} {path}: {value}");
        }
    }
    for (method, path, body) in probes(&tim, json!([GroupId::VIEWER])) {
        if path.ends_with("/invite") {
            continue; // no SMTP in the test kernel
        }
        let (status, value) = call_as(app, &editor, method, &path, body).await;
        assert!(status.is_success(), "{method} {path}: {status} {value}");
    }
}

// ---------- B4f1 take-over guard, adversarial (test/b4t2) ----------

/// A new group `name` with `permissions`, made by the token holder.
async fn group(app: &Router, name: &str, permissions: Value) -> GroupId {
    let body = json!({ "name": name, "permissions": permissions });
    let (status, group) = call_as(app, TOKEN, "POST", "/api/v1/groups", Some(body)).await;
    assert_eq!(status, StatusCode::CREATED, "{group}");
    id_of(&group).parse().unwrap()
}

/// A new workspace `name` seen by `groups`, made by the token holder.
async fn workspace(app: &Router, name: &str, groups: &[GroupId]) {
    let body = json!({ "name": name, "group_ids": groups });
    let (status, workspace) = call_as(app, TOKEN, "POST", "/api/v1/workspaces", Some(body)).await;
    assert_eq!(status, StatusCode::CREATED, "{workspace}");
}

/// `caller` sets member `target`'s password: the status.
async fn set_password(app: &Router, caller: &str, target: &str) -> StatusCode {
    let path = format!("/api/v1/members/{target}/password");
    let body = json!({ "password": "a password set by someone else", "must_change": false });
    call_as(app, caller, "PUT", &path, Some(body)).await.0
}

/// Owners always pass the guard (other owners included); `manage_groups`
/// passes it for anyone but an owner, stronger or wider than the caller.
#[tokio::test]
async fn acl_owners_and_group_managers_act_on_anyone_below_owner() {
    let k = kernel().await;
    let app = &k.app;
    let secret = group(app, "Secret team", json!([])).await;
    workspace(app, "X", &[secret]).await;
    let managers = json!(["view_members", "edit_members", "manage_groups"]);
    let managers = group(app, "Managers", managers).await;
    let (olive, olive_id) = member(&k.store, "olive", &[]).await;
    let (_, oscar) = member(&k.store, "oscar", &[]).await;
    for id in [&olive_id, &oscar] {
        let path = format!("/api/v1/members/{id}/owner");
        let body = Some(json!({ "is_owner": true }));
        let (status, value) = call_as(app, TOKEN, "PUT", &path, body).await;
        assert_eq!(status, StatusCode::OK, "{value}");
    }
    let (_, ada) = member(&k.store, "ada", &[GroupId::ADMIN]).await;
    let (_, sam) = member(&k.store, "sam", &[secret]).await;
    let (gm, _) = member(&k.store, "gm", &[GroupId::VIEWER, managers]).await;

    for target in [&ada, &sam, &oscar] {
        let status = set_password(app, &olive, target).await;
        assert_eq!(status, StatusCode::OK, "owner on {target}");
    }
    for verb in ["deactivate", "activate"] {
        let path = format!("/api/v1/members/{oscar}/{verb}");
        let (status, value) = call_as(app, &olive, "POST", &path, None).await;
        assert_eq!(status, StatusCode::OK, "owner {verb}s an owner: {value}");
    }
    // gm can't do Admin's work nor see X, but manages groups.
    assert_eq!(set_password(app, &gm, &ada).await, StatusCode::OK, "Admin");
    assert_eq!(set_password(app, &gm, &sam).await, StatusCode::OK, "on X");
    let status = set_password(app, &gm, &oscar).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "an owner");
}

/// The guard compares what the target can do and see with the caller:
/// an equal peer and someone with no groups are in reach; one more workspace,
/// or `access_all_workspaces` the caller lacks, is not. A caller who sees
/// every workspace reaches a member on any workspace.
#[tokio::test]
async fn acl_take_over_guard_peers_groupless_and_every_workspace() {
    let k = kernel().await;
    let app = &k.app;
    let editors = group(app, "Editors", json!(["view_members", "edit_members"])).await;
    let secret = group(app, "Secret team", json!([])).await;
    workspace(app, "X", &[secret]).await;
    let everywhere = group(app, "Everywhere", json!(["access_all_workspaces"])).await;
    let (ed, _) = member(&k.store, "ed", &[GroupId::VIEWER, editors]).await;
    let (_, peer) = member(&k.store, "peer", &[GroupId::VIEWER, editors]).await;
    let (_, wider) = member(&k.store, "wider", &[GroupId::VIEWER, editors, secret]).await;
    let (_, nobody) = member(&k.store, "nobody", &[]).await;
    let (_, tim) = member(&k.store, "tim", &[GroupId::VIEWER]).await;
    let (_, wanda) = member(&k.store, "wanda", &[everywhere]).await;
    let (_, sam) = member(&k.store, "sam", &[secret]).await;
    // An editor who sees no workspace at all, and one who sees every workspace.
    let (lone, _) = member(&k.store, "lone", &[editors]).await;
    let (seer, _) = member(&k.store, "seer", &[editors, everywhere]).await;

    for (caller, target, want, what) in [
        (&ed, &peer, StatusCode::OK, "equal peer"),
        (&ed, &wider, StatusCode::FORBIDDEN, "peer who also sees X"),
        (&ed, &nobody, StatusCode::OK, "no groups"),
        (&ed, &wanda, StatusCode::FORBIDDEN, "access_all_workspaces"),
        (
            &lone,
            &nobody,
            StatusCode::OK,
            "no groups, from no workspace",
        ),
        (
            &lone,
            &tim,
            StatusCode::FORBIDDEN,
            "Main, from no workspace",
        ),
        (
            &seer,
            &wanda,
            StatusCode::OK,
            "every workspace, from every workspace",
        ),
        (&seer, &sam, StatusCode::OK, "X, from every workspace"),
        (&seer, &tim, StatusCode::OK, "Main, from every workspace"),
    ] {
        assert_eq!(set_password(app, caller, target).await, want, "{what}");
    }
}

/// A group attached to several workspaces opens all of them: a caller who sees
/// only some can't grant it, nor act on its members; a group on workspaces
/// they see stays grantable.
#[tokio::test]
async fn acl_group_on_several_workspaces_needs_every_workspace_seen() {
    let k = kernel().await;
    let app = &k.app;
    let on_x = group(app, "On X", json!([])).await;
    let both = group(app, "On X and Y", json!([])).await;
    workspace(app, "X", &[on_x, both]).await;
    workspace(app, "Y", &[both]).await;
    let recruiters = json!(["view_members", "edit_members", "create_members"]);
    let recruiters = group(app, "Recruiters", recruiters).await;
    let (rex, _) = member(&k.store, "rex", &[recruiters, on_x]).await;
    let (_, tim) = member(&k.store, "tim", &[]).await;
    let (_, bo) = member(&k.store, "bo", &[both]).await;
    let (_, xena) = member(&k.store, "xena", &[on_x]).await;

    let path = format!("/api/v1/members/{tim}/groups");
    let into_both = Some(json!({ "group_ids": [both] }));
    let (status, _) = call_as(app, &rex, "PUT", &path, into_both).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "into X and Y");
    let into_x = Some(json!({ "group_ids": [on_x] }));
    let (status, value) = call_as(app, &rex, "PUT", &path, into_x).await;
    assert_eq!(status, StatusCode::OK, "into X only: {value}");
    let new = json!({ "email": "nu@corp.io", "username": "nu", "group_ids": [both],
        "password": "a long enough password" });
    let (status, _) = call_as(app, &rex, "POST", "/api/v1/members", Some(new)).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "a new member in X and Y");
    assert_eq!(set_password(app, &rex, &bo).await, StatusCode::FORBIDDEN);
    assert_eq!(set_password(app, &rex, &xena).await, StatusCode::OK);
}
