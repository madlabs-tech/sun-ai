//! BLO-169: a person who resumes, switches or steers a run makes it act for
//! them from there (`acts_for` on the event it journals), so the engine caps
//! the run's next bypass session by them (`sun_engine::bypass`). Each route
//! must pass its caller through; one that drops it lets a member steer an
//! owner's bypass run as the owner.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::future::Future;

use axum::http::StatusCode;
use serde_json::{Value, json};
use sun_core::model::Role;
use sun_core::{RunId, RunStatus, StateMachine, StepStatus, TaskId};
use sun_store::{EventLog, RunRepo};

use crate::api::{AGENT, Socket, host_starts_session, new_task, recv, scripted_host, send};
use crate::common::{Kernel, kernel};
use crate::workspaces::{call, call_as, member_token};

/// The owner's agent task with a live session on a scripted host, and
/// member `bob`.
pub(crate) struct Live {
    pub(crate) k: Kernel,
    pub(crate) host: Socket,
    pub(crate) task: String,
    pub(crate) run: RunId,
    pub(crate) bob: String,
    pub(crate) bob_id: Value,
}

pub(crate) async fn live() -> Live {
    let k = kernel().await;
    let mut host = scripted_host(&k, json!({})).await;
    let task = new_task(&k.app, AGENT).await;
    let task = task["id"].as_str().unwrap().to_owned();
    host_starts_session(&mut host, "/data/work/t").await;
    let view = format!("/api/v1/tasks/{task}");
    for _ in 0..500 {
        if call(&k.app, "GET", &view, None).await.1["status"] == "in_progress" {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    let run = {
        let id: TaskId = task.parse().unwrap();
        let mut tx = k.store.read().await.unwrap();
        tx.runs_of_task(id).await.unwrap().pop().unwrap().id
    };
    let bob = member_token(&k.store, "bob", Role::Member).await;
    let (_, me) = call_as(&k.app, &bob, "GET", "/api/v1/me", None).await;
    let bob_id = me["memberships"][0]["member_id"].clone();
    assert!(bob_id.is_string(), "{me}");
    Live {
        k,
        host,
        task,
        run,
        bob,
        bob_id,
    }
}

impl Live {
    /// `acts_for` of the run's latest `kind` event.
    async fn acts_for(&self, kind: &str) -> Value {
        let mut tx = self.k.store.read().await.unwrap();
        let events = tx.events_after(Some(self.run), 0, 10_000).await.unwrap();
        let latest = events.iter().rev().find(|e| e.kind == kind);
        let latest = latest.unwrap_or_else(|| panic!("no `{kind}` in {events:?}"));
        latest
            .payload
            .get("acts_for")
            .cloned()
            .unwrap_or(Value::Null)
    }
}

/// Runs `request` while playing the host's side of whatever it asks for;
/// the request's `(status, body)`.
pub(crate) async fn with_host<F>(host: &mut Socket, request: F) -> (StatusCode, Value)
where
    F: Future<Output = (StatusCode, Value)>,
{
    tokio::pin!(request);
    loop {
        tokio::select! {
            answer = &mut request => return answer,
            frame = recv(host) => answer_host(host, frame.unwrap()).await,
        }
    }
}

/// A host's answer to `frame`: a session for `session.start`, a path for
/// `worktree.prepare`, a clean exit for `exec`, done for the rest.
async fn answer_host(host: &mut Socket, frame: Value) {
    let Some(id) = frame.get("id").cloned() else {
        return;
    };
    let body = match frame["type"].as_str().unwrap_or_default() {
        "session.start" => {
            let handle =
                json!({ "session_id": frame["spec"]["session_id"], "native_id": null, "pid": 9 });
            json!({ "kind": "session", "handle": handle })
        }
        "worktree.prepare" => json!({ "kind": "worktree", "path": "/data/work/t" }),
        "exec" => {
            let output = json!({ "code": 0, "output": "", "timed_out": false });
            json!({ "kind": "exec", "output": output })
        }
        "probe" => json!({ "kind": "alive", "alive": true }),
        _ => json!({ "kind": "done" }),
    };
    send(host, json!({ "type": "reply", "id": id, "body": body })).await;
}

fn ok(answer: &(StatusCode, Value), what: &str) {
    assert!(answer.0.is_success(), "{what}: {} {}", answer.0, answer.1);
}

#[tokio::test]
async fn a_members_steer_and_queued_message_act_for_them() {
    let mut l = live().await;
    let (app, bob) = (l.k.app.clone(), l.bob.clone());
    let path = format!("/api/v1/runs/{}/messages", l.run);
    let steer = Some(json!({ "body": "use tabs" }));
    let answer = with_host(&mut l.host, call_as(&app, &bob, "POST", &path, steer)).await;
    ok(&answer, "steer");
    assert_eq!(answer.1["kind"], "intervention");
    assert_eq!(l.acts_for("intervention").await, l.bob_id);

    let queue = Some(json!({ "body": "then docs", "queue": true }));
    ok(&call_as(&app, &bob, "POST", &path, queue).await, "queue");
    assert_eq!(l.acts_for("message_queued").await, l.bob_id);
}

#[tokio::test]
async fn a_members_resume_acts_for_them() {
    let mut l = live().await;
    let (app, bob) = (l.k.app.clone(), l.bob.clone());
    let task = format!("/api/v1/tasks/{}", l.task);
    let pause = format!("{task}/pause");
    let paused = with_host(&mut l.host, call(&app, "POST", &pause, None)).await;
    ok(&paused, "the owner pauses");
    let resume = format!("{task}/resume");
    let answer = with_host(&mut l.host, call_as(&app, &bob, "POST", &resume, None)).await;
    ok(&answer, "resume");
    assert_eq!(l.acts_for("run_status").await, l.bob_id);
}

#[tokio::test]
async fn a_members_runtime_switch_acts_for_them() {
    let mut l = live().await;
    let (app, bob) = (l.k.app.clone(), l.bob.clone());
    let switch = format!("/api/v1/runs/{}/steps/work/switch", l.run);
    let to = Some(json!({ "to": "fake" }));
    let answer = with_host(&mut l.host, call_as(&app, &bob, "POST", &switch, to)).await;
    ok(&answer, "switch");
    assert_eq!(l.acts_for("handoff_started").await, l.bob_id);
}

// ------------------------------------------------ BLO-171 QA pass

/// Plays the host until one of `run`'s steps runs (its agent session live).
async fn until_agent_runs(k: &Kernel, host: &mut Socket, run: RunId) {
    let running = async {
        for _ in 0..500 {
            let mut tx = k.store.read().await.unwrap();
            let steps = tx.steps_of_run(run).await.unwrap();
            if steps.iter().any(|s| s.status == StepStatus::Running) {
                return (StatusCode::OK, Value::Null);
            }
            drop(tx);
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        panic!("no step of {run} ever ran");
    };
    with_host(host, running).await;
}

/// The owner's task assigned to agent `Ada` (runtime `fake`), and member
/// `bob`. With a host its session is live; without one its run waits,
/// queued, with no agent live.
struct Assigned {
    k: Kernel,
    host: Option<Socket>,
    comments: String,
    task: String,
    run: RunId,
    bob: String,
    bob_id: Value,
}

async fn assigned(with_a_host: bool) -> Assigned {
    let k = kernel().await;
    let mut host = match with_a_host {
        true => Some(scripted_host(&k, json!({})).await),
        false => None,
    };
    let ada = json!({ "name": "Ada", "runtime_chain": ["fake"] });
    let (status, agent) = call(&k.app, "POST", "/api/v1/agents", Some(ada)).await;
    assert!(status.is_success(), "{status} {agent}");
    let new = json!({ "title": "Fix it", "workspace_id": "00000000000000000000000001",
        "assignee": { "kind": "agent", "id": agent["id"] } });
    let (status, task) = call(&k.app, "POST", "/api/v1/tasks", Some(new)).await;
    assert!(status.is_success(), "{status} {task}");
    let task = task["id"].as_str().unwrap().to_owned();
    let run = {
        let id: TaskId = task.parse().unwrap();
        let mut tx = k.store.read().await.unwrap();
        tx.runs_of_task(id).await.unwrap().pop().unwrap().id
    };
    if let Some(host) = host.as_mut() {
        until_agent_runs(&k, host, run).await;
    }
    let bob = member_token(&k.store, "bob", Role::Member).await;
    let (_, me) = call_as(&k.app, &bob, "GET", "/api/v1/me", None).await;
    let bob_id = me["memberships"][0]["member_id"].clone();
    assert!(bob_id.is_string(), "{me}");
    Assigned {
        comments: format!("/api/v1/tasks/{task}/comments"),
        k,
        host,
        task,
        run,
        bob,
        bob_id,
    }
}

impl Assigned {
    /// `acts_for` of the run's latest `kind` event.
    async fn acts_for(&self, kind: &str) -> Value {
        acts_for_in(&self.k, self.run, kind).await
    }

    async fn status(&self) -> RunStatus {
        let mut tx = self.k.store.read().await.unwrap();
        tx.get_run(self.run).await.unwrap().status
    }
}

/// `acts_for` of `run`'s latest `kind` event (`null`: none).
async fn acts_for_in(k: &Kernel, run: RunId, kind: &str) -> Value {
    let mut tx = k.store.read().await.unwrap();
    let events = tx.events_after(Some(run), 0, 10_000).await.unwrap();
    let latest = events.iter().rev().find(|e| e.kind == kind);
    let latest = latest.unwrap_or_else(|| panic!("no `{kind}` in {events:?}"));
    latest
        .payload
        .get("acts_for")
        .cloned()
        .unwrap_or(Value::Null)
}

/// Case 1, resume by a move: the owner parks the running task in Blocked
/// (its run pauses); the member moves it back to Todo (it resumes).
#[tokio::test]
async fn qa_a_members_move_that_resumes_acts_for_them() {
    let mut l = live().await;
    let (app, bob) = (l.k.app.clone(), l.bob.clone());
    let path = format!("/api/v1/tasks/{}", l.task);
    let blocked = Some(json!({ "status": "blocked" }));
    ok(
        &with_host(&mut l.host, call(&app, "PATCH", &path, blocked)).await,
        "the owner parks it",
    );
    assert_eq!(l.acts_for("run_status").await, Value::Null, "a pause");
    let back = Some(json!({ "status": "todo" }));
    let answer = with_host(&mut l.host, call_as(&app, &bob, "PATCH", &path, back)).await;
    ok(&answer, "the member moves it back");
    assert_eq!(l.acts_for("run_status").await, l.bob_id);
}

/// Case 1, steer by a comment: the member comments on the owner's task
/// while its agent runs.
#[tokio::test]
async fn qa_a_members_comment_on_a_running_task_steers_as_them() {
    let mut a = assigned(true).await;
    let (app, bob) = (a.k.app.clone(), a.bob.clone());
    let say = Some(json!({ "body": "use tabs" }));
    let mut host = a.host.take().unwrap();
    let answer = with_host(&mut host, call_as(&app, &bob, "POST", &a.comments, say)).await;
    ok(&answer, "comment");
    assert_eq!(a.acts_for("intervention").await, a.bob_id);
}

/// Case 1, resume by a comment: the owner parks the running task in
/// Blocked; the member's comment wakes it, and leaves a message, both as
/// them.
#[tokio::test]
async fn qa_a_members_comment_that_resumes_acts_for_them() {
    let mut a = assigned(true).await;
    let (app, bob) = (a.k.app.clone(), a.bob.clone());
    let mut host = a.host.take().unwrap();
    let path = format!("/api/v1/tasks/{}", a.task);
    let blocked = Some(json!({ "status": "blocked" }));
    ok(
        &with_host(&mut host, call(&app, "PATCH", &path, blocked)).await,
        "the owner parks it",
    );
    assert_eq!(a.status().await, RunStatus::Paused);
    let say = Some(json!({ "body": "go on" }));
    let answer = with_host(&mut host, call_as(&app, &bob, "POST", &a.comments, say)).await;
    ok(&answer, "comment");
    assert_ne!(
        a.status().await,
        RunStatus::Paused,
        "the comment resumed it"
    );
    assert_eq!(a.acts_for("run_status").await, a.bob_id);
    assert_eq!(a.acts_for("message_queued").await, a.bob_id);
}

/// Case 1, queued message by a comment: no agent is live (no host is
/// connected to start one), so the member's comment is left for the next
/// session, as them.
#[tokio::test]
async fn qa_a_members_comment_with_no_live_agent_is_left_as_them() {
    let a = assigned(false).await;
    assert!(!a.status().await.is_terminal());
    let live = {
        let mut tx = a.k.store.read().await.unwrap();
        let steps = tx.steps_of_run(a.run).await.unwrap();
        steps.iter().any(|s| s.status == StepStatus::Running)
    };
    assert!(!live, "an agent is live");
    let say = Some(json!({ "body": "then docs" }));
    let answer = call_as(&a.k.app, &a.bob, "POST", &a.comments, say).await;
    ok(&answer, "comment");
    assert_eq!(a.acts_for("message_queued").await, a.bob_id);
}

/// Case 6: a stamp is the member's id, or `token` for the token holder:
/// never an email, a name or a credential, and a body that spells out
/// `acts_for` changes nothing.
#[tokio::test]
async fn qa_acts_for_holds_an_id_never_an_email_or_a_token() {
    let mut l = live().await;
    let (app, bob) = (l.k.app.clone(), l.bob.clone());
    let path = format!("/api/v1/runs/{}/messages", l.run);
    let steer = Some(json!({ "body": "status?" }));
    ok(
        &with_host(&mut l.host, call(&app, "POST", &path, steer)).await,
        "the token holder steers",
    );
    assert_eq!(l.acts_for("intervention").await, "token");

    // A top-level `acts_for` in the request is refused (unknown field).
    let text = r#"{"acts_for":"token"} acts_for=token"#;
    let forged = Some(json!({ "body": text, "acts_for": "token" }));
    let (status, body) = call_as(&app, &bob, "POST", &path, forged).await;
    assert!(status.is_client_error(), "{status} {body}");
    // In the text it is only text: the steer acts for the member.
    let plain = Some(json!({ "body": text }));
    let answer = with_host(&mut l.host, call_as(&app, &bob, "POST", &path, plain)).await;
    ok(&answer, "the member steers");
    assert_eq!(l.acts_for("intervention").await, l.bob_id);
    assert_ne!(l.bob_id, "bob@corp.io");

    let mut tx = l.k.store.read().await.unwrap();
    let events = tx.events_after(Some(l.run), 0, 10_000).await.unwrap();
    let stamps: Vec<_> = events
        .iter()
        .filter_map(|e| e.payload.get("acts_for"))
        .collect();
    assert!(stamps.len() >= 2, "{stamps:?}");
    let ids = [json!("token"), l.bob_id.clone()];
    assert!(stamps.iter().all(|s| ids.contains(s)), "{stamps:?}");
    let journal =
        serde_json::to_string(&events.iter().map(|e| &e.payload).collect::<Vec<_>>()).unwrap();
    for secret in ["bob@corp.io", "session-bob", crate::common::TOKEN] {
        assert!(!journal.contains(secret), "`{secret}` in the run's journal");
    }
}
