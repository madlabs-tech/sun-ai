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
use sun_core::{RunId, TaskId};
use sun_store::{EventLog, RunRepo};

use crate::api::{AGENT, Socket, host_starts_session, new_task, recv, scripted_host, send};
use crate::common::{Kernel, kernel};
use crate::workspaces::{call, call_as, member_token};

/// The owner's agent task with a live session on a scripted host, and
/// member `bob`.
struct Live {
    k: Kernel,
    host: Socket,
    task: String,
    run: RunId,
    bob: String,
    bob_id: Value,
}

async fn live() -> Live {
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
async fn with_host<F>(host: &mut Socket, request: F) -> (StatusCode, Value)
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
