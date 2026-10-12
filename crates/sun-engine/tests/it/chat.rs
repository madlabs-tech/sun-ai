//! Chat users (Slack, Jira buttons) act as the Sun member with the same
//! email, and only by email: manual chat links are gone, so a stale
//! `chat_links` settings row, a missing email, or a mismatched one resolves
//! to nobody.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::json;
use sun_core::model::Role;
use sun_core::model::{Budget, Run, Task, Workflow};
use sun_core::{HostId, ModEvent, RunId, RunStatus, StepStatus, TaskStatus, WorkflowId};
use sun_engine::{Engine, EngineConfig, WatchdogConfig};
use sun_store::{
    EventLog, MemberRepo, MembershipRepo, RunRepo, SettingRepo, Store, TaskRepo, WorkflowRepo,
};

use crate::queue::{NoHost, NoMods};

const REVIEW: &str =
    "name: review\nsteps:\n  - id: review\n    type: approval\n    who: [role:slack]\n";

/// An engine with one run waiting at the `review` approval.
async fn waiting_run(dir: &std::path::Path) -> (Store, Engine, RunId) {
    let store = Store::open(dir).await.unwrap();
    let parsed = sun_workflow::parse(REVIEW).unwrap();
    let workflow = Workflow {
        id: WorkflowId::generate(),
        workspace_id: sun_core::WorkspaceId::MAIN,
        name: parsed.name.clone(),
    };
    let task = Task {
        title: "t".into(),
        workflow_id: Some(workflow.id),
        status: TaskStatus::Todo,
        ..sun_core::model::Task::new(sun_core::WorkspaceId::MAIN, "")
    };
    let run = Run {
        id: RunId::generate(),
        task_id: task.id,
        workflow_id: workflow.id,
        workflow_version: 1,
        status: RunStatus::Queued,
        end_reason: None,
        budget: Budget::default(),
        repo_ids: Vec::new(),
    };
    let mut tx = store.begin().await.unwrap();
    tx.insert_workflow(&workflow).await.unwrap();
    tx.save_workflow_version(workflow.id, REVIEW, &parsed.hash)
        .await
        .unwrap();
    tx.insert_task(&task).await.unwrap();
    tx.insert_run(&run).await.unwrap();
    tx.commit().await.unwrap();
    let config = EngineConfig {
        host: HostId::generate(),
        adapter: "fake".into(),
        delivery_timeout: Duration::from_secs(5),
        watchdog: WatchdogConfig::default(),
    };
    let (engine, _task) = Engine::start(store.clone(), Arc::new(NoHost), Arc::new(NoMods), config)
        .await
        .unwrap();
    engine.start_run(run.id).await.unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let steps = store
            .read()
            .await
            .unwrap()
            .steps_of_run(run.id)
            .await
            .unwrap();
        if steps
            .iter()
            .any(|s| s.status == StepStatus::WaitingApproval)
        {
            break;
        }
        assert!(Instant::now() < deadline, "never waiting for approval");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    (store, engine, run.id)
}

/// A Slack approve click by chat user `user` reporting `email` (`None`: the
/// field is missing). No channel, so a refusal queues no reply.
fn click(run: RunId, user: &str, email: Option<&str>) -> ModEvent {
    let mut payload = json!({"approval_id": format!("{run}/review"), "decision": "approved",
        "user": user});
    if let Some(email) = email {
        payload["email"] = json!(email);
    }
    ModEvent {
        mod_name: "slack".into(),
        topic: "approval".into(),
        payload,
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn chat_user_resolves_by_matching_email_only() {
    let dir = tempfile::tempdir().unwrap();
    let (store, engine, run) = waiting_run(dir.path()).await;

    // ana and nomail may approve; nomail has no email yet (pre-invite member).
    let mut tx = store.begin().await.unwrap();
    let ana = tx
        .upsert_member("https://idp", "a", "ana", Some("ana@acme.io"), false)
        .await
        .unwrap();
    let nomail = tx
        .upsert_member("https://idp", "n", "nomail", None, false)
        .await
        .unwrap();
    for id in [ana.id, nomail.id] {
        tx.add_member(sun_core::WorkspaceId::MAIN, id, Role::Member)
            .await
            .unwrap();
    }
    // A link stored before T15 removed manual linking: it must stay inert.
    tx.put_setting(
        "chat_links",
        &json!({ "slack:U1": ana.id, "slack:U3": nomail.id }),
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();

    // `chat_member`: exact email in any case; nothing else matches.
    let name = |m: Option<sun_core::model::Member>| m.map(|m| m.username);
    for email in ["ana@acme.io", "ANA@Acme.IO"] {
        assert_eq!(
            name(engine.chat_member(email).await.unwrap()),
            Some("ana".into())
        );
    }
    for email in [
        "",
        "ana",
        "eve@else.io",
        "ana@acme.io.evil",
        " ana@acme.io",
        "@",
    ] {
        assert_eq!(
            name(engine.chat_member(email).await.unwrap()),
            None,
            "{email:?}"
        );
    }

    // Clicks the old links would have resolved, now refused: no email, an
    // empty one, a mismatched one.
    for (user, email) in [
        ("U1", None),
        ("U1", Some("")),
        ("U1", Some("eve@else.io")),
        ("U3", None),
    ] {
        assert!(
            !engine
                .decide_from_chat(&click(run, user, email))
                .await
                .unwrap(),
            "{user} {email:?} decided"
        );
    }
    let approvals = || async {
        let mut tx = store.read().await.unwrap();
        tx.count_events("approval").await.unwrap()
    };
    assert_eq!(approvals().await, 0);

    // The matching email decides, as ana.
    assert!(
        engine
            .decide_from_chat(&click(run, "U9", Some("Ana@acme.io")))
            .await
            .unwrap()
    );
    assert_eq!(approvals().await, 1);
}

/// A deactivated member can't sign in and their sessions end; a chat click
/// or thread reply must not let them act either (`chat_member` skips them).
#[tokio::test(flavor = "multi_thread")]
async fn deactivated_member_is_refused_from_chat() {
    let dir = tempfile::tempdir().unwrap();
    let (store, engine, run) = waiting_run(dir.path()).await;
    let mut tx = store.begin().await.unwrap();
    let dee = tx
        .upsert_member("https://idp", "d", "dee", Some("dee@acme.io"), false)
        .await
        .unwrap();
    tx.add_member(sun_core::WorkspaceId::MAIN, dee.id, Role::Member)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    engine
        .set_member_active(dee.id, false, "test")
        .await
        .unwrap();

    // A thread reply: refused is `Ok(false)`; admitted reaches `steer`,
    // which errs here (no agent runs at an approval).
    let reply = ModEvent {
        mod_name: "slack".into(),
        topic: "steer".into(),
        payload: json!({"key": format!("{run}:review"), "text": "go", "user": "U4",
            "email": "dee@acme.io", "channel": "", "ts": "1.1"}),
    };
    let steered = engine.steer_from_chat(&reply).await;
    assert!(
        matches!(steered, Ok(false)),
        "a deactivated member's reply reached steer: {steered:?}"
    );
    let decided = engine
        .decide_from_chat(&click(run, "U4", Some("dee@acme.io")))
        .await
        .unwrap();
    assert!(!decided, "a deactivated member approved from chat");
}
