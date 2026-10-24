//! BLO-199: the team-access test pass through the real kernel. Domain
//! look-alikes never join a group; an email change drops membership on the
//! next request; a removed member loses a shared host; `/hosts` on a shared
//! host shows no other workspace's tasks; the terminal policy is enforced on
//! the WebSocket connect itself; a group editor grant adds and revokes; and
//! a terminal on the kernel's own host shows none of the kernel's `SUN_*`.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::time::Duration;

use axum::http::StatusCode;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use sun_core::model::Role;
use sun_core::{MemberId, TaskId, WorkspaceId};
use sun_store::{LoginRepo, MemberRepo};

use crate::api::send;
use crate::common::{TOKEN, kernel};
use crate::editor_grants::member_id;
use crate::shared_hosts::{owner_of_a_workspace, person, team};
use crate::terminal::{Screen, opened, read_until, refused, type_line, world};
use crate::workspaces::{call_as, member_token};

/// Group names `token` is in now (`GET /groups/mine`).
async fn mine(app: &axum::Router, token: &str) -> Vec<String> {
    let (status, list) = call_as(app, token, "GET", "/api/v1/groups/mine", None).await;
    assert_eq!(status, StatusCode::OK, "{list}");
    list.as_array()
        .unwrap()
        .iter()
        .map(|g| g["name"].as_str().unwrap().to_owned())
        .collect()
}

/// `/me.can_terminal` for `token`.
async fn can_terminal(app: &axum::Router, token: &str) -> bool {
    let (status, me) = call_as(app, token, "GET", "/api/v1/me", None).await;
    assert_eq!(status, StatusCode::OK, "{me}");
    me["can_terminal"] == true
}

async fn set_policy(app: &axum::Router, policy: Value) {
    let (status, body) =
        call_as(app, TOKEN, "PUT", "/api/v1/settings/terminal", Some(policy)).await;
    assert_eq!(status, StatusCode::OK, "{body}");
}

async fn change_email(store: &sun_store::Store, id: MemberId, email: &str) {
    let mut tx = store.begin().await.unwrap();
    tx.set_member_email(id, email).await.unwrap();
    tx.commit().await.unwrap();
}

async fn verify(store: &sun_store::Store, id: MemberId) {
    let mut tx = store.begin().await.unwrap();
    tx.verify_email(id).await.unwrap();
    tx.commit().await.unwrap();
}

#[tokio::test]
async fn only_a_verified_exact_domain_joins_a_group() {
    let k = kernel().await;
    let app = &k.app;
    let body = json!({ "name": "Madlabs team", "domains": ["madlabs.tech"] });
    let (status, group) = call_as(app, TOKEN, "POST", "/api/v1/groups", Some(body)).await;
    assert_eq!(status, StatusCode::CREATED, "{group}");
    let g = group["id"].as_str().unwrap().to_owned();
    set_policy(app, json!({ "mode": "groups", "groups": [g] })).await;

    let (upper, _) = person(&k, "X@MADLABS.TECH", true).await;
    assert_eq!(
        mine(app, &upper).await,
        ["Madlabs team"],
        "case-insensitive"
    );
    assert!(can_terminal(app, &upper).await);
    let (unverified, _) = person(&k, "u@madlabs.tech", false).await;
    let (prefix, _) = person(&k, "x@evilmadlabs.tech", true).await;
    let (suffix, _) = person(&k, "x@madlabs.tech.evil.com", true).await;
    let (sub, _) = person(&k, "x@sub.madlabs.tech", true).await;
    for (who, token) in [
        ("unverified", &unverified),
        ("prefix look-alike", &prefix),
        ("suffix look-alike", &suffix),
        ("unlisted subdomain", &sub),
    ] {
        assert!(mine(app, token).await.is_empty(), "{who}");
        assert!(!can_terminal(app, token).await, "{who}");
    }

    // Listing the subdomain lets it in, and nothing else.
    let edit = json!({ "domains": ["madlabs.tech", "sub.madlabs.tech"] });
    let (status, body) = call_as(
        app,
        TOKEN,
        "PATCH",
        &format!("/api/v1/groups/{g}"),
        Some(edit),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(mine(app, &sub).await, ["Madlabs team"]);
    for token in [&unverified, &prefix, &suffix] {
        assert!(mine(app, token).await.is_empty());
    }
}

#[tokio::test]
async fn an_email_change_clears_verification_and_drops_membership_on_the_next_request() {
    let t = team().await;
    let app = &t.k.app;
    assert_eq!(
        t.as_owner("PUT", &t.share(), None).await.0,
        StatusCode::NO_CONTENT
    );
    set_policy(app, json!({ "mode": "groups", "groups": [t.group] })).await;
    let ana =
        t.k.store
            .read()
            .await
            .unwrap()
            .member_by_email("ana@madlabs.tech")
            .await
            .unwrap()
            .unwrap()
            .id;
    assert_eq!(mine(app, &t.ana.token).await, ["Madlabs team"]);
    assert!(can_terminal(app, &t.ana.token).await);

    // A case-only change is the same inbox: still verified, still in.
    change_email(&t.k.store, ana, "Ana@MadLabs.tech").await;
    assert_eq!(mine(app, &t.ana.token).await, ["Madlabs team"]);

    // Another address on the same domain is a new inbox, not proven yet.
    change_email(&t.k.store, ana, "ana2@madlabs.tech").await;
    let (_, me) = call_as(app, &t.ana.token, "GET", "/api/v1/me", None).await;
    assert_eq!(me["email_verified"], false, "{me}");
    assert!(mine(app, &t.ana.token).await.is_empty());
    assert!(!can_terminal(app, &t.ana.token).await);
    let host = json!(t.host);
    assert!(!t.attachable(&t.ana).await.iter().any(|h| h["id"] == host));
    let (status, _) = call_as(app, &t.ana.token, "PUT", &t.attach(&t.ana), None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    verify(&t.k.store, ana).await;
    assert_eq!(mine(app, &t.ana.token).await, ["Madlabs team"]);
}

#[tokio::test]
async fn an_email_set_through_the_api_is_not_verified() {
    let k = kernel().await;
    let app = &k.app;
    let body = json!({ "name": "Madlabs team", "domains": ["madlabs.tech"] });
    assert_eq!(
        call_as(app, TOKEN, "POST", "/api/v1/groups", Some(body))
            .await
            .0,
        StatusCode::CREATED
    );
    // A member from before invites, without an email: the one caller
    // `PUT /me/email` lets set one.
    let token = "session-noemail";
    let hash: String = Sha256::digest(token.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    let mut tx = k.store.begin().await.unwrap();
    let member = tx
        .upsert_member("https://idp", "noemail", "noemail", None, false)
        .await
        .unwrap();
    tx.insert_login_session(
        &hash,
        member.id,
        chrono::Utc::now() + chrono::Duration::hours(1),
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();

    let set = json!({ "email": "x@madlabs.tech" });
    let (status, body) = call_as(app, token, "PUT", "/api/v1/me/email", Some(set)).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (_, me) = call_as(app, token, "GET", "/api/v1/me", None).await;
    assert_eq!(me["email_verified"], false, "{me}");
    assert!(mine(app, token).await.is_empty(), "claimed, not proven");
    // And it can't be swapped again once set.
    let again = json!({ "email": "y@madlabs.tech" });
    assert_eq!(
        call_as(app, token, "PUT", "/api/v1/me/email", Some(again))
            .await
            .0,
        StatusCode::FORBIDDEN
    );
}

#[tokio::test]
async fn a_member_removed_from_the_group_loses_the_shared_host() {
    let t = team().await;
    let app = &t.k.app;
    let host = json!(t.host);
    assert_eq!(
        t.as_owner("PUT", &t.share(), None).await.0,
        StatusCode::NO_CONTENT
    );
    // carl@corp.io, added by hand.
    let carl =
        t.k.store
            .read()
            .await
            .unwrap()
            .member_by_email("carl@corp.io")
            .await
            .unwrap()
            .unwrap()
            .id;
    let member = format!("/api/v1/groups/{}/members/{carl}", t.group);
    assert_eq!(
        t.as_owner("POST", &member, None).await.0,
        StatusCode::NO_CONTENT
    );
    assert!(t.attachable(&t.carl).await.iter().any(|h| h["id"] == host));
    let (status, body) = call_as(app, &t.carl.token, "PUT", &t.attach(&t.carl), None).await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");
    assert_eq!(t.hosts_of(&t.carl).await, std::slice::from_ref(&host));

    assert_eq!(
        t.as_owner("DELETE", &member, None).await.0,
        StatusCode::NO_CONTENT
    );
    assert!(!t.attachable(&t.carl).await.iter().any(|h| h["id"] == host));
    assert!(
        t.hosts_of(&t.carl).await.is_empty(),
        "the link held only through the share"
    );
    let (status, _) = call_as(app, &t.carl.token, "PUT", &t.attach(&t.carl), None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    // Ana, still in by domain, keeps hers.
    call_as(app, &t.ana.token, "PUT", &t.attach(&t.ana), None).await;
    assert_eq!(t.hosts_of(&t.ana).await, std::slice::from_ref(&host));
}

#[tokio::test(flavor = "multi_thread")]
async fn hosts_on_a_shared_host_show_no_other_workspaces_tasks() {
    let t = team().await;
    let app = &t.k.app;
    let bob = owner_of_a_workspace(&t.k, "bob@madlabs.tech", true).await;
    t.as_owner("PUT", &t.share(), None).await;
    for who in [&t.ana, &bob] {
        let (status, body) = call_as(app, &who.token, "PUT", &t.attach(who), None).await;
        assert_eq!(status, StatusCode::NO_CONTENT, "{body}");
    }
    let mut socket = t.connect().await;
    let ana_task = t.agent_task(&t.ana).await;
    let bob_task = t.agent_task(&bob).await;
    let tasks = json!([[ana_task, 10], [bob_task, 20]]);
    send(&mut socket, json!({ "type": "disk", "tasks": tasks })).await;

    let disk_of = |token: String, workspace: Option<String>| {
        let app = app.clone();
        async move {
            let path = match workspace {
                Some(w) => format!("/api/v1/hosts?workspace={w}"),
                None => "/api/v1/hosts".to_owned(),
            };
            let (status, list) = call_as(&app, &token, "GET", &path, None).await;
            assert_eq!(status, StatusCode::OK, "{list}");
            let host = list
                .as_array()
                .unwrap()
                .iter()
                .find(|h| h["id"] == json!(t.host))
                .cloned();
            host.map(|h| h["disk"].clone()).unwrap_or(Value::Null)
        }
    };
    // The disk frame lands asynchronously: wait for the owner's view.
    let until = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let seen = disk_of(TOKEN.to_owned(), None).await;
        if seen.as_array().is_some_and(|d| !d.is_empty()) {
            break;
        }
        assert!(
            tokio::time::Instant::now() < until,
            "no disk report: {seen}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let ana_sees = disk_of(t.ana.token.clone(), Some(t.ana.workspace.clone())).await;
    assert_eq!(
        ana_sees,
        json!([{ "task_id": ana_task, "bytes": 10 }]),
        "ana"
    );
    let bob_sees = disk_of(bob.token.clone(), Some(bob.workspace.clone())).await;
    assert_eq!(
        bob_sees,
        json!([{ "task_id": bob_task, "bytes": 20 }]),
        "bob"
    );
    // Ana asking with bob's workspace is not hers to ask about.
    let path = format!("/api/v1/hosts?workspace={}", bob.workspace);
    let (status, body) = call_as(app, &t.ana.token, "GET", &path, None).await;
    assert!(
        status == StatusCode::NOT_FOUND || status == StatusCode::FORBIDDEN,
        "{status} {body}"
    );
    assert!(!body.to_string().contains(&bob_task.to_string()), "{body}");
}

#[tokio::test(flavor = "multi_thread")]
async fn the_terminal_policy_is_enforced_on_the_websocket_connect() {
    let w = world().await;
    let app = &w.k.app;
    // A workspace owner of Main (a public sign-up), a team member with a
    // `terminal` grant by group, and a second deployment owner.
    let public = member_token(&w.k.store, "pub", Role::Owner).await;
    let tia = member_token(&w.k.store, "tia", Role::Member).await;
    let tia_id: MemberId = member_id(&w.k.store, "tia").await.parse().unwrap();
    change_email(&w.k.store, tia_id, "tia@madlabs.tech").await;
    verify(&w.k.store, tia_id).await;
    let boss = "session-boss";
    let hash: String = Sha256::digest(boss.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    let mut tx = w.k.store.begin().await.unwrap();
    let member = tx
        .upsert_member("https://idp", "boss", "boss", Some("boss@corp.io"), true)
        .await
        .unwrap();
    tx.insert_login_session(
        &hash,
        member.id,
        chrono::Utc::now() + chrono::Duration::hours(1),
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();

    let body = json!({ "name": "Madlabs team", "domains": ["madlabs.tech"] });
    let (_, group) = call_as(app, TOKEN, "POST", "/api/v1/groups", Some(body)).await;
    let g = group["id"].as_str().unwrap().to_owned();
    let grants = format!("/api/v1/workspaces/{}/editor/grants", WorkspaceId::MAIN);
    for capability in ["editor.read", "terminal"] {
        let grant = json!({ "subject_type": "group", "subject_id": g, "capability": capability });
        assert_eq!(
            call_as(app, TOKEN, "POST", &grants, Some(grant)).await.0,
            StatusCode::CREATED
        );
    }
    for token in [&public, &tia, &boss.to_owned()] {
        w.open_editor(token).await;
    }

    // Granted mode: the workspace owner may.
    let mut ws = w.connect(&public, "").await.unwrap();
    let pty = opened(&mut ws).await;
    drop(ws);

    set_policy(app, json!({ "mode": "groups", "groups": [g] })).await;
    // The flip applies to the next connect, a reattach to their own PTY included.
    assert_eq!(
        refused(w.connect(&public, "").await),
        StatusCode::FORBIDDEN,
        "workspace owner"
    );
    let reattach = w.connect(&public, &format!("&pty_id={pty}")).await;
    assert_eq!(refused(reattach), StatusCode::FORBIDDEN, "reattach");
    let mut ws = w.connect(&tia, "").await.unwrap();
    opened(&mut ws).await;
    drop(ws);
    let mut ws = w.connect(boss, "").await.unwrap();
    opened(&mut ws).await;
    drop(ws);

    // Back to granted: the next connect lets the owner in again.
    set_policy(app, json!({ "mode": "granted", "groups": [] })).await;
    let mut ws = w.connect(&public, "").await.unwrap();
    opened(&mut ws).await;
}

#[tokio::test]
async fn a_group_editor_grant_adds_and_revokes() {
    let t = team().await;
    let app = &t.k.app;
    // Ana and bob in the team; bob a plain member of ana's workspace.
    let (bob, bob_id) = person(&t.k, "bob@madlabs.tech", true).await;
    let w: WorkspaceId = t.ana.workspace.parse().unwrap();
    let mut tx = t.k.store.begin().await.unwrap();
    sun_store::MembershipRepo::add_member(&mut tx, w, bob_id, Role::Member)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    let repo = crate::common::local_repo(&t.k, w, "/srv/ana").await;
    let me = format!("/api/v1/workspaces/{w}/repos/{repo}/editor/me");
    let caps = |token: String| {
        let (app, me) = (app.clone(), me.clone());
        async move {
            let (status, body) = call_as(&app, &token, "GET", &me, None).await;
            assert_eq!(status, StatusCode::OK, "{body}");
            serde_json::from_value::<Vec<String>>(body["capabilities"].clone()).unwrap()
        }
    };
    assert!(caps(bob.clone()).await.is_empty());

    let grants = format!("/api/v1/workspaces/{w}/editor/grants");
    let grant =
        json!({ "subject_type": "group", "subject_id": t.group, "capability": "editor.write" });
    // Bob, a plain member, can't grant; ana, the owner, can.
    assert_eq!(
        call_as(app, &bob, "POST", &grants, Some(grant.clone()))
            .await
            .0,
        StatusCode::FORBIDDEN
    );
    let (status, made) = call_as(app, &t.ana.token, "POST", &grants, Some(grant)).await;
    assert_eq!(status, StatusCode::CREATED, "{made}");
    assert_eq!(made["subject_type"], "group", "{made}");
    assert_eq!(caps(bob.clone()).await, ["editor.read", "editor.write"]);
    // x (unverified look-alike) is not in ana's workspace at all: 404.
    let (status, _) = call_as(app, &t.x.token, "GET", &me, None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    let revoke = format!("{grants}/{}", made["id"].as_str().unwrap());
    assert_eq!(
        call_as(app, &t.ana.token, "DELETE", &revoke, None).await.0,
        StatusCode::NO_CONTENT
    );
    assert!(
        caps(bob.clone()).await.is_empty(),
        "revoked on the next request"
    );
    assert_eq!(
        call_as(app, &t.ana.token, "DELETE", &revoke, None).await.0,
        StatusCode::NOT_FOUND
    );
    let (_, listed) = call_as(app, &t.ana.token, "GET", &grants, None).await;
    assert_eq!(listed, json!([]));

    // A grant to a group that doesn't exist is refused.
    let ghost = json!({ "subject_type": "group", "subject_id": sun_core::GroupId::generate(), "capability": "terminal" });
    let (status, _) = call_as(app, &t.ana.token, "POST", &grants, Some(ghost)).await;
    assert!(status.is_client_error(), "{status}");
}

/// Set in the child process [`a_terminal_on_the_kernel_host_shows_no_kernel_env`] starts.
const CHILD: &str = "TEAM_ACCESS_CHILD";
/// The master key the child kernel runs with.
const MASTER_KEY: &str = "7e57ab1e0000000000000000000000000000000000000000000000000000beef";

/// The child's half: a kernel with its real in-process host (as `sun serve
/// --with-host`), `SUN_MASTER_KEY` in its env; `env` and
/// `/proc/self/environ` typed in a terminal, printed.
#[tokio::test(flavor = "multi_thread")]
async fn kernel_env_child() {
    if std::env::var_os(CHILD).is_none() {
        return;
    }
    let w = world().await;
    let owner = member_token(&w.k.store, "own", Role::Owner).await;
    w.open_editor(&owner).await;
    let mut ws = w.connect(&owner, "").await.unwrap();
    opened(&mut ws).await;
    type_line(
        &mut ws,
        "env; tr '\\0' '\\n' < /proc/self/environ; echo END-$((40+2))",
    )
    .await;
    let mut screen = Screen::default();
    read_until(&mut ws, &mut screen, |s| s.output.contains("END-42")).await;
    println!("PTY<<<{}>>>PTY", screen.output);
}

#[test]
fn a_terminal_on_the_kernel_host_shows_no_kernel_env() {
    let exe = std::env::current_exe().unwrap();
    let out = std::process::Command::new(exe)
        .args([
            "--exact",
            "team_access::kernel_env_child",
            "--nocapture",
            "--test-threads=1",
        ])
        .env(CHILD, "1")
        .env("SUN_MASTER_KEY", MASTER_KEY)
        .env("PLAIN_VAR", "kept")
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    assert!(
        out.status.success(),
        "{stdout}{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let (_, rest) = stdout.split_once("PTY<<<").expect("the child ran");
    let (seen, _) = rest.split_once(">>>PTY").unwrap();
    assert!(
        seen.contains("PLAIN_VAR=kept"),
        "the rest is inherited: {seen}"
    );
    for secret in ["SUN_MASTER_KEY", MASTER_KEY] {
        assert!(
            !seen.contains(secret),
            "the terminal shows {secret}: {seen}"
        );
    }
}

/// BLO-197 bug: `list_hosts` treats a task the kernel no longer knows as
/// visible (`workspace_access::visible` answers `true` on `NotFound`), so a
/// deleted task's folder on a shared host shows its id and size to every
/// workspace the host is attached to.
#[tokio::test(flavor = "multi_thread")]
async fn hosts_on_a_shared_host_hide_folders_of_deleted_tasks() {
    let t = team().await;
    let app = &t.k.app;
    t.as_owner("PUT", &t.share(), None).await;
    call_as(app, &t.ana.token, "PUT", &t.attach(&t.ana), None).await;
    let mut socket = t.connect().await;
    let ana_task = t.agent_task(&t.ana).await;
    // Left on disk by a task of another workspace, since deleted.
    let gone = TaskId::generate();
    let tasks = json!([[ana_task, 10], [gone, 30]]);
    send(&mut socket, json!({ "type": "disk", "tasks": tasks })).await;
    let path = format!("/api/v1/hosts?workspace={}", t.ana.workspace);
    let until = tokio::time::Instant::now() + Duration::from_secs(10);
    let disk = loop {
        let (status, list) = call_as(app, &t.ana.token, "GET", &path, None).await;
        assert_eq!(status, StatusCode::OK, "{list}");
        let disk = list[0]["disk"].clone();
        if disk.as_array().is_some_and(|d| !d.is_empty()) {
            break disk;
        }
        assert!(tokio::time::Instant::now() < until, "no disk report");
        tokio::time::sleep(Duration::from_millis(50)).await;
    };
    assert_eq!(disk, json!([{ "task_id": ana_task, "bytes": 10 }]));
}
