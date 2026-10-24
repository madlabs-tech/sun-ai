//! BLO-197: a deployment owner shares a host with a group; a member of the
//! group attaches it to a workspace they run and their agents run there.
//! Nobody else sees or attaches it (404), an unverified `x@madlabs.tech`
//! included; after an unshare nothing new is placed on it, while running
//! sessions finish; and the kernel's own host is shared only on explicit
//! confirmation.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::time::Duration;

use axum::http::StatusCode;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use sun_core::model::Host;
use sun_core::{GroupId, HostId, MemberId, StateMachine, TaskId};
use sun_store::{EventLog, HostRepo, LoginRepo, MemberRepo, RunRepo, SettingRepo};

use crate::api::{AGENT, Socket, connect_host, hello, recv};
use crate::bypass_acts_for::answer_host;
use crate::common::{Kernel, SECRET, TOKEN, kernel};
use crate::workspaces::call_as;

/// A deployment with the team host (offline), the group "Madlabs team"
/// (`madlabs.tech`), and three people, each owning a workspace of their own.
struct Team {
    k: Kernel,
    host: HostId,
    group: GroupId,
    /// Verified `ana@madlabs.tech`: in the group.
    ana: Person,
    /// Unverified `x@madlabs.tech`: anyone may sign up with that.
    x: Person,
    /// Verified `carl@corp.io`.
    carl: Person,
}

struct Person {
    token: String,
    workspace: String,
}

/// A session token of a new member with `email`, verified or not.
async fn person(k: &Kernel, email: &str, verified: bool) -> (String, MemberId) {
    let token = format!("session-{email}");
    let hash: String = Sha256::digest(token.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    let mut tx = k.store.begin().await.unwrap();
    let member = tx.invite_member(email, email, false).await.unwrap();
    if verified {
        tx.verify_email(member.id).await.unwrap();
    }
    let expires = chrono::Utc::now() + chrono::Duration::hours(1);
    tx.insert_login_session(&hash, member.id, expires)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    (token, member.id)
}

/// A member with `email` and a workspace they own.
async fn owner_of_a_workspace(k: &Kernel, email: &str, verified: bool) -> Person {
    let (token, _) = person(k, email, verified).await;
    let body = json!({ "name": format!("ws of {email}") });
    let (status, workspace) =
        call_as(&k.app, &token, "POST", "/api/v1/workspaces", Some(body)).await;
    assert_eq!(status, StatusCode::CREATED, "{workspace}");
    let workspace = workspace["id"].as_str().unwrap().to_owned();
    Person { token, workspace }
}

async fn team() -> Team {
    let k = kernel().await;
    let host = HostId::generate();
    let mut tx = k.store.begin().await.unwrap();
    tx.insert_host(&Host {
        id: host,
        name: "team-box".into(),
        capabilities: json!({}),
        enrolled_by: None,
    })
    .await
    .unwrap();
    tx.commit().await.unwrap();
    let body = json!({ "name": "Madlabs team", "domains": ["madlabs.tech"] });
    let (status, group) = call_as(&k.app, TOKEN, "POST", "/api/v1/groups", Some(body)).await;
    assert_eq!(status, StatusCode::CREATED, "{group}");
    let group: GroupId = group["id"].as_str().unwrap().parse().unwrap();
    let ana = owner_of_a_workspace(&k, "ana@madlabs.tech", true).await;
    let x = owner_of_a_workspace(&k, "x@madlabs.tech", false).await;
    let carl = owner_of_a_workspace(&k, "carl@corp.io", true).await;
    Team {
        k,
        host,
        group,
        ana,
        x,
        carl,
    }
}

impl Team {
    async fn as_owner(&self, method: &str, path: &str, body: Option<Value>) -> (StatusCode, Value) {
        call_as(&self.k.app, TOKEN, method, path, body).await
    }

    fn share(&self) -> String {
        format!("/api/v1/hosts/{}/shares/{}", self.host, self.group)
    }

    fn attach(&self, who: &Person) -> String {
        format!("/api/v1/workspaces/{}/hosts/{}", who.workspace, self.host)
    }

    /// The ids `GET /hosts/attachable` lists for `who`'s workspace.
    async fn attachable(&self, who: &Person) -> Vec<Value> {
        let path = format!("/api/v1/hosts/attachable?workspace={}", who.workspace);
        let (status, list) = call_as(&self.k.app, &who.token, "GET", &path, None).await;
        assert_eq!(status, StatusCode::OK, "{list}");
        list.as_array().unwrap().clone()
    }

    /// The host ids `GET /hosts?workspace=` lists for `who`'s workspace.
    async fn hosts_of(&self, who: &Person) -> Vec<Value> {
        let path = format!("/api/v1/hosts?workspace={}", who.workspace);
        let (status, list) = call_as(&self.k.app, &who.token, "GET", &path, None).await;
        assert_eq!(status, StatusCode::OK, "{list}");
        list.as_array()
            .unwrap()
            .iter()
            .map(|h| h["id"].clone())
            .collect()
    }

    /// The team host online, reporting the `fake` runtime ready.
    async fn connect(&self) -> Socket {
        let key: String = Sha256::digest(format!("{SECRET}:{}", self.host).as_bytes())
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        connect_host(&self.k.app, self.host, hello(self.host, &key)).await
    }

    /// A new agent task in `who`'s workspace, as them.
    async fn agent_task(&self, who: &Person) -> TaskId {
        let app = &self.k.app;
        let flow = json!({ "source_yaml": AGENT, "workspace_id": who.workspace });
        let (status, workflow) =
            call_as(app, &who.token, "POST", "/api/v1/workflows", Some(flow)).await;
        assert_eq!(status, StatusCode::CREATED, "{workflow}");
        let task = json!({ "title": "Fix it", "workflow_id": workflow["id"] });
        let (status, task) = call_as(app, &who.token, "POST", "/api/v1/tasks", Some(task)).await;
        assert_eq!(status, StatusCode::CREATED, "{task}");
        task["id"].as_str().unwrap().parse().unwrap()
    }

    /// Where the task's run was placed, once it was; `None` after `wait`.
    async fn placed(&self, task: TaskId, wait: Duration) -> Option<Value> {
        let until = tokio::time::Instant::now() + wait;
        while tokio::time::Instant::now() < until {
            let mut tx = self.k.store.read().await.unwrap();
            if let Some(run) = tx.runs_of_task(task).await.unwrap().pop() {
                let events = tx.events_after(Some(run.id), 0, 10_000).await.unwrap();
                if let Some(e) = events.iter().find(|e| e.kind == "run_placed") {
                    return Some(e.payload["host"].clone());
                }
            }
            drop(tx);
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        None
    }
}

#[tokio::test]
async fn a_group_member_attaches_a_shared_host_and_a_run_lands_on_it() {
    let t = team().await;
    let app = &t.k.app;
    let host = json!(t.host);
    // Not shared yet: not hers to attach.
    assert!(!t.attachable(&t.ana).await.iter().any(|h| h["id"] == host));
    let (status, _) = call_as(app, &t.ana.token, "PUT", &t.attach(&t.ana), None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    let (status, body) = t.as_owner("PUT", &t.share(), None).await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");
    let path = format!("/api/v1/hosts/{}/shares", t.host);
    let (_, shares) = t.as_owner("GET", &path, None).await;
    assert_eq!(shares["kernel"], false);
    assert_eq!(shares["shares"][0]["group_id"], json!(t.group));

    // She sees it (and only it: the kernel's Main host isn't hers), attaches it.
    let list = t.attachable(&t.ana).await;
    assert_eq!(list.len(), 1, "{list:?}");
    assert_eq!(list[0]["id"], host);
    assert_eq!(list[0]["via"], "share");
    assert_eq!(list[0]["kernel"], false);
    assert_eq!(list[0]["attached"], false);
    let (status, body) = call_as(app, &t.ana.token, "PUT", &t.attach(&t.ana), None).await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");
    assert_eq!(t.attachable(&t.ana).await[0]["attached"], true);
    assert_eq!(t.hosts_of(&t.ana).await, std::slice::from_ref(&host));

    let _socket = t.connect().await;
    let task = t.agent_task(&t.ana).await;
    let placed = t.placed(task, Duration::from_secs(20)).await;
    assert_eq!(placed, Some(host), "her run goes to the shared host");
}

#[tokio::test]
async fn nobody_outside_the_group_sees_or_attaches_it() {
    let t = team().await;
    let app = &t.k.app;
    let host = json!(t.host);
    let (status, _) = t.as_owner("PUT", &t.share(), None).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    // The unverified look-alike and an outsider, each owner of a workspace.
    for who in [&t.x, &t.carl] {
        assert!(!t.attachable(who).await.iter().any(|h| h["id"] == host));
        let (status, body) = call_as(app, &who.token, "PUT", &t.attach(who), None).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
        assert!(t.hosts_of(who).await.is_empty());
        // Sharing is the deployment owner's.
        let (status, _) = call_as(app, &who.token, "PUT", &t.share(), None).await;
        assert_eq!(status, StatusCode::FORBIDDEN);
    }
    // Attached by her, it still names only workspaces each caller sees.
    let (status, _) = call_as(app, &t.ana.token, "PUT", &t.attach(&t.ana), None).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let path = format!("/api/v1/hosts/attachable?workspace={}", t.carl.workspace);
    let (_, list) = call_as(app, &t.carl.token, "GET", &path, None).await;
    assert_eq!(list, json!([]));
    // Once x proves the inbox, x is in.
    let x_id = {
        let mut tx = t.k.store.read().await.unwrap();
        tx.member_by_email("x@madlabs.tech")
            .await
            .unwrap()
            .unwrap()
            .id
    };
    let mut tx = t.k.store.begin().await.unwrap();
    tx.verify_email(x_id).await.unwrap();
    tx.commit().await.unwrap();
    assert!(t.attachable(&t.x).await.iter().any(|h| h["id"] == host));
}

#[tokio::test(flavor = "multi_thread")]
async fn after_unsharing_nothing_new_is_placed_and_running_sessions_finish() {
    let t = team().await;
    let app = &t.k.app;
    let host = json!(t.host);
    t.as_owner("PUT", &t.share(), None).await;
    call_as(app, &t.ana.token, "PUT", &t.attach(&t.ana), None).await;
    let mut socket = t.connect().await;
    let first = t.agent_task(&t.ana).await;
    // Play the host until the first run's session started.
    loop {
        let frame = recv(&mut socket)
            .await
            .expect("the kernel talks to the host");
        let started = frame["type"] == "session.start";
        answer_host(&mut socket, frame).await;
        if started {
            break;
        }
    }
    assert_eq!(
        t.placed(first, Duration::from_secs(5)).await,
        Some(host.clone())
    );

    let (status, body) = t.as_owner("DELETE", &t.share(), None).await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");
    let (status, _) = t.as_owner("DELETE", &t.share(), None).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "not shared any more");
    assert!(!t.attachable(&t.ana).await.iter().any(|h| h["id"] == host));
    assert!(
        t.hosts_of(&t.ana).await.is_empty(),
        "her workspace had it only through the share"
    );

    let second = t.agent_task(&t.ana).await;
    assert_eq!(
        t.placed(second, Duration::from_secs(3)).await,
        None,
        "nothing new"
    );
    // The running session was left alone: no stop for it.
    let quiet = tokio::time::timeout(Duration::from_secs(2), recv(&mut socket)).await;
    if let Ok(Some(frame)) = quiet {
        assert_ne!(frame["type"], "session.stop", "{frame}");
    }
    let run =
        t.k.store
            .read()
            .await
            .unwrap()
            .runs_of_task(first)
            .await
            .unwrap();
    assert!(!run[0].status.is_terminal(), "{:?}", run[0].status);

    // Shared again, the link holds again and the waiting run goes there.
    t.as_owner("PUT", &t.share(), None).await;
    assert_eq!(t.placed(second, Duration::from_secs(20)).await, Some(host));
    // Deleting the group takes its shares, and her link, with it.
    let (status, _) = t
        .as_owner("DELETE", &format!("/api/v1/groups/{}", t.group), None)
        .await;
    assert!(status.is_success(), "{status}");
    assert!(t.hosts_of(&t.ana).await.is_empty());
}

#[tokio::test]
async fn sharing_the_kernel_host_needs_an_explicit_confirmation() {
    let t = team().await;
    let kernel_host = t.k.host;
    let mut tx = t.k.store.begin().await.unwrap();
    tx.put_setting(sun_engine::KERNEL_HOST, &json!(kernel_host))
        .await
        .unwrap();
    tx.commit().await.unwrap();
    let share = format!("/api/v1/hosts/{kernel_host}/shares/{}", t.group);

    let (status, body) = t.as_owner("PUT", &share, None).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert!(body.to_string().contains("SUN_MASTER_KEY"), "{body}");
    let refused = json!({ "confirm_kernel_host": false });
    let (status, _) = t.as_owner("PUT", &share, Some(refused)).await;
    assert_eq!(status, StatusCode::CONFLICT);
    let (_, shares) = t
        .as_owner("GET", &format!("/api/v1/hosts/{kernel_host}/shares"), None)
        .await;
    assert_eq!(shares, json!({ "kernel": true, "shares": [] }));
    assert!(
        !t.attachable(&t.ana)
            .await
            .iter()
            .any(|h| h["id"] == json!(kernel_host))
    );

    let (_, hosts) = t.as_owner("GET", "/api/v1/hosts", None).await;
    let listed = hosts
        .as_array()
        .unwrap()
        .iter()
        .find(|h| h["id"] == json!(kernel_host));
    assert_eq!(listed.unwrap()["kernel"], true, "the UI can tell it apart");

    let confirmed = json!({ "confirm_kernel_host": true });
    let (status, body) = t.as_owner("PUT", &share, Some(confirmed)).await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");
    let ana_sees = t.attachable(&t.ana).await;
    let entry = ana_sees
        .iter()
        .find(|h| h["id"] == json!(kernel_host))
        .unwrap();
    assert_eq!(entry["kernel"], true);
    let (_, audit) = t.as_owner("GET", "/api/v1/audit", None).await;
    assert!(
        audit.to_string().contains("shared with group Madlabs team"),
        "{audit}"
    );

    // A missing host or group is 404, either way round.
    let missing = format!("/api/v1/hosts/{}/shares/{}", HostId::generate(), t.group);
    assert_eq!(
        t.as_owner("PUT", &missing, None).await.0,
        StatusCode::NOT_FOUND
    );
    let missing = format!("/api/v1/hosts/{}/shares/{}", t.host, GroupId::generate());
    assert_eq!(
        t.as_owner("PUT", &missing, None).await.0,
        StatusCode::NOT_FOUND
    );
}
