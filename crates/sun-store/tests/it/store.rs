//! Store contract tests against a real SQLite file in a temp dir.
// clippy's allow-unwrap-in-tests covers `#[test]` fns, not the helpers here.
#![allow(clippy::unwrap_used)]

use serde_json::json;
use sqlx::migrate::Migrate;
use sqlx::{Connection, SqliteConnection};
use sun_core::model::{Budget, OutboxItem, OutboxStatus, Run, Step, Task, Workflow};
use sun_core::{
    Error, IdemKey, OutboxId, RunId, RunStatus, SessionId, StepId, StepKey, StepStatus, SunEvent,
    TaskId, TaskStatus, WorkflowId,
};
use sun_store::{
    EventLog, NewEvent, OutboxRepo, RunRepo, Store, StoreError, TaskRepo, Tx, WorkflowRepo,
};

struct Fixture {
    _dir: tempfile::TempDir,
    store: Store,
    run_id: RunId,
    step_id: StepId,
}

/// A store with one workflow (v1), task, run and queued step committed.
async fn fixture() -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).await.unwrap();
    let workflow = Workflow {
        id: WorkflowId::generate(),
        workspace_id: sun_core::WorkspaceId::MAIN,
        name: "default".into(),
    };
    let task = Task {
        title: "fix".into(),
        body: "".into(),
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
        repo_ids: Vec::new(),
        budget: Budget {
            turns: Some(10),
            ..Budget::default()
        },
    };
    let step = Step {
        id: StepId::generate(),
        run_id: run.id,
        step_key: key("implement"),
        attempt: 1,
        status: StepStatus::Queued,
        runtime_id: None,
        credential_id: None,
        session_id: None,
        started_at: None,
        ended_at: None,
    };
    let mut tx = store.begin().await.unwrap();
    tx.insert_workflow(&workflow).await.unwrap();
    tx.save_workflow_version(workflow.id, "name: default", "h")
        .await
        .unwrap();
    tx.insert_task(&task).await.unwrap();
    tx.insert_run(&run).await.unwrap();
    tx.insert_step(&step).await.unwrap();
    tx.commit().await.unwrap();
    Fixture {
        _dir: dir,
        store,
        run_id: run.id,
        step_id: step.id,
    }
}

fn key(text: &str) -> StepKey {
    StepKey::new(text).unwrap()
}

fn event(run_id: RunId, kind: &str) -> NewEvent {
    NewEvent::new(run_id, Some(key("implement")), kind, json!({ "n": kind }))
}

fn outbox_item(run_id: RunId, action: &str) -> OutboxItem {
    OutboxItem {
        id: OutboxId::generate(),
        run_id: Some(run_id),
        idem_key: IdemKey::new(run_id, &key("implement"), action),
        target: "github.open_pr".into(),
        payload: json!({ "branch": "sun/x" }),
        status: OutboxStatus::Pending,
        result: None,
    }
}

async fn all_events(tx: &mut Tx, run_id: RunId) -> Vec<i64> {
    tx.events_after(Some(run_id), 0, 1000)
        .await
        .unwrap()
        .iter()
        .map(|e| e.seq)
        .collect()
}

#[tokio::test]
async fn one_transaction_commits_state_events_and_outbox_together() {
    let f = fixture().await;
    let mut live = f.store.subscribe();

    // Rolled back: nothing visible, nothing broadcast.
    let mut tx = f.store.begin().await.unwrap();
    tx.set_step_status(f.step_id, StepStatus::Dispatched)
        .await
        .unwrap();
    tx.append_event(event(f.run_id, "step_status"))
        .await
        .unwrap();
    tx.enqueue(&outbox_item(f.run_id, "a")).await.unwrap();
    drop(tx);
    let mut read = f.store.read().await.unwrap();
    assert_eq!(
        read.get_step(f.step_id).await.unwrap().status,
        StepStatus::Queued
    );
    assert!(all_events(&mut read, f.run_id).await.is_empty());
    assert!(
        read.outbox_with_status(OutboxStatus::Pending, 10)
            .await
            .unwrap()
            .is_empty()
    );
    drop(read);
    assert!(live.try_recv().is_err());

    // Committed: all three land, events are broadcast, seq has no gap from the rollback.
    let mut tx = f.store.begin().await.unwrap();
    tx.set_step_status(f.step_id, StepStatus::Dispatched)
        .await
        .unwrap();
    let first = tx
        .append_event(event(f.run_id, "step_status"))
        .await
        .unwrap();
    let agent = SunEvent::FileChanged {
        path: "src/a.rs".into(),
    };
    let second = tx.append_event(NewEvent::agent(f.run_id, key("implement"), &agent).unwrap());
    let second = second.await.unwrap();
    assert!(tx.enqueue(&outbox_item(f.run_id, "a")).await.unwrap());
    tx.commit().await.unwrap();

    assert_eq!((first.seq, second.seq), (1, 2));
    assert_eq!(second.kind, "file_changed");
    assert_eq!(live.recv().await.unwrap(), first);
    assert_eq!(live.recv().await.unwrap(), second);
    let mut read = f.store.read().await.unwrap();
    assert_eq!(
        read.get_step(f.step_id).await.unwrap().status,
        StepStatus::Dispatched
    );
    assert_eq!(all_events(&mut read, f.run_id).await, vec![1, 2]);
    assert_eq!(
        read.outbox_with_status(OutboxStatus::Pending, 10)
            .await
            .unwrap()
            .len(),
        1
    );
}

#[tokio::test]
async fn statuses_only_move_along_state_machine_edges() {
    let f = fixture().await;
    let mut tx = f.store.begin().await.unwrap();
    let err = tx
        .set_step_status(f.step_id, StepStatus::Succeeded)
        .await
        .unwrap_err();
    assert!(matches!(
        err,
        StoreError::Core(Error::InvalidTransition { .. })
    ));

    for next in [
        StepStatus::Dispatched,
        StepStatus::Running,
        StepStatus::Verifying,
    ] {
        tx.set_step_status(f.step_id, next).await.unwrap();
    }
    let step = tx.get_step(f.step_id).await.unwrap();
    assert!(step.started_at.is_some() && step.ended_at.is_none());
    assert_eq!(
        tx.set_step_status(f.step_id, StepStatus::Succeeded)
            .await
            .unwrap(),
        StepStatus::Verifying
    );
    assert!(tx.get_step(f.step_id).await.unwrap().ended_at.is_some());

    assert_eq!(tx.active_runs().await.unwrap().len(), 1);
    tx.set_run_status(f.run_id, RunStatus::Running, None)
        .await
        .unwrap();
    tx.set_run_status(f.run_id, RunStatus::Succeeded, Some("gates passed"))
        .await
        .unwrap();
    let run = tx.get_run(f.run_id).await.unwrap();
    assert_eq!(
        (run.status, run.end_reason.as_deref()),
        (RunStatus::Succeeded, Some("gates passed"))
    );
    assert_eq!(run.budget.turns, Some(10));
    assert!(tx.active_runs().await.unwrap().is_empty());

    let missing = tx
        .set_task_status(TaskId::generate(), TaskStatus::InProgress)
        .await
        .unwrap_err();
    assert!(matches!(missing, StoreError::Core(Error::NotFound(_))));
}

#[tokio::test]
async fn host_events_are_stored_exactly_once_and_events_are_append_only() {
    let f = fixture().await;
    let session = SessionId::generate();
    let mut tx = f.store.begin().await.unwrap();
    assert!(
        tx.append_host_event(session, 1, event(f.run_id, "a"))
            .await
            .unwrap()
            .is_some()
    );
    assert!(
        tx.append_host_event(session, 2, event(f.run_id, "b"))
            .await
            .unwrap()
            .is_some()
    );
    // Redelivery after a reconnect.
    assert!(
        tx.append_host_event(session, 1, event(f.run_id, "a"))
            .await
            .unwrap()
            .is_none()
    );
    // Same host seq from another session is a different event.
    assert!(
        tx.append_host_event(SessionId::generate(), 1, event(f.run_id, "c"))
            .await
            .unwrap()
            .is_some()
    );
    tx.commit().await.unwrap();

    let mut tx = f.store.begin().await.unwrap();
    assert_eq!(all_events(&mut tx, f.run_id).await, vec![1, 2, 3]);
    let page = tx.events_after(None, 1, 1).await.unwrap();
    assert_eq!(page.iter().map(|e| e.seq).collect::<Vec<_>>(), vec![2]);
    let update = sqlx::query("UPDATE event SET kind = 'x'");
    let mut conn = SqliteConnection::connect(&format!(
        "sqlite://{}",
        f._dir.path().join("sun.db").display()
    ))
    .await
    .unwrap();
    drop(tx);
    let err = update.execute(&mut conn).await.unwrap_err();
    assert!(err.to_string().contains("append-only"), "{err}");
}

#[tokio::test]
async fn outbox_enqueue_is_idempotent_by_key() {
    let f = fixture().await;
    let mut tx = f.store.begin().await.unwrap();
    let item = outbox_item(f.run_id, "open_pr");
    assert!(tx.enqueue(&item).await.unwrap());
    // A retried step builds the same idem key with a fresh id.
    assert!(!tx.enqueue(&outbox_item(f.run_id, "open_pr")).await.unwrap());
    tx.set_outbox_status(item.id, OutboxStatus::Delivered, Some(&json!({ "pr": 7 })))
        .await
        .unwrap();
    let delivered = tx
        .outbox_with_status(OutboxStatus::Delivered, 10)
        .await
        .unwrap();
    assert_eq!(delivered.len(), 1);
    assert_eq!(delivered[0].idem_key, item.idem_key);
    assert_eq!(delivered[0].result, Some(json!({ "pr": 7 })));
}

#[tokio::test]
async fn workflow_versions_count_up_and_tasks_queue_by_priority() {
    let f = fixture().await;
    let mut tx = f.store.begin().await.unwrap();
    let run = tx.get_run(f.run_id).await.unwrap();
    let v2 = tx
        .save_workflow_version(run.workflow_id, "name: v2", "h")
        .await
        .unwrap();
    assert_eq!(v2.version, 2);
    assert_eq!(
        tx.latest_workflow_version(run.workflow_id).await.unwrap(),
        Some(v2)
    );
    assert_eq!(
        tx.get_workflow_version(run.workflow_id, 1)
            .await
            .unwrap()
            .source_yaml,
        "name: default"
    );

    let mut urgent = tx.get_task(run.task_id).await.unwrap();
    urgent.id = TaskId::generate();
    urgent.priority = sun_core::model::Priority::Urgent;
    assert_eq!(
        tx.insert_task(&urgent).await.unwrap(),
        Some(2),
        "the next number"
    );
    let queued = tx.tasks().await.unwrap();
    assert_eq!(
        queued.iter().map(|t| t.id).collect::<Vec<_>>(),
        vec![urgent.id, run.task_id]
    );
}

#[tokio::test]
async fn concurrent_writers_are_serialized_without_gaps() {
    let f = fixture().await;
    let tasks: Vec<_> = (0..20)
        .map(|_| {
            let store = f.store.clone();
            let run_id = f.run_id;
            tokio::spawn(async move {
                let mut tx = store.begin().await.unwrap();
                tx.append_event(event(run_id, "tick")).await.unwrap();
                tx.commit().await.unwrap();
            })
        })
        .collect();
    for task in tasks {
        task.await.unwrap();
    }
    let mut read = f.store.read().await.unwrap();
    assert_eq!(
        all_events(&mut read, f.run_id).await,
        (1..=20).collect::<Vec<_>>()
    );
    // Read transactions are read-only.
    assert!(read.append_event(event(f.run_id, "x")).await.is_err());
}

#[tokio::test]
async fn backs_up_before_migrating_an_existing_database_only() {
    let dir = tempfile::tempdir().unwrap();
    let backups = dir.path().join("backups");

    // Fresh install: nothing to back up; reopening an up-to-date DB: no backup either.
    drop(Store::open(dir.path()).await.unwrap());
    drop(Store::open(dir.path()).await.unwrap());
    assert!(!backups.exists());

    // An older install: migrations table present, our migration not yet applied.
    let old = tempfile::tempdir().unwrap();
    let url = format!("sqlite://{}?mode=rwc", old.path().join("sun.db").display());
    let mut conn = SqliteConnection::connect(&url).await.unwrap();
    conn.ensure_migrations_table().await.unwrap();
    sqlx::query("CREATE TABLE legacy (x INTEGER)")
        .execute(&mut conn)
        .await
        .unwrap();
    conn.close().await.unwrap();

    let store = Store::open(old.path()).await.unwrap();
    let files: Vec<_> = std::fs::read_dir(old.path().join("backups"))
        .unwrap()
        .collect();
    assert_eq!(files.len(), 1);
    let backup = files.into_iter().next().unwrap().unwrap().path();
    let mut copy = SqliteConnection::connect(&format!("sqlite://{}", backup.display()))
        .await
        .unwrap();
    let legacy: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM sqlite_master WHERE name = 'legacy'")
            .fetch_one(&mut copy)
            .await
            .unwrap();
    assert_eq!(legacy, 1);

    // Manual backup works on a live store too.
    store.backup(&old.path().join("manual.db")).await.unwrap();
    assert!(old.path().join("manual.db").exists());
}

#[tokio::test]
async fn wal_and_fts5_are_available() {
    let f = fixture().await;
    let mut conn = SqliteConnection::connect(&format!(
        "sqlite://{}",
        f._dir.path().join("sun.db").display()
    ))
    .await
    .unwrap();
    let mode: String = sqlx::query_scalar("PRAGMA journal_mode")
        .fetch_one(&mut conn)
        .await
        .unwrap();
    assert_eq!(mode, "wal");
    sqlx::query("CREATE VIRTUAL TABLE temp.probe USING fts5(body)")
        .execute(&mut conn)
        .await
        .unwrap();
}

#[tokio::test]
async fn correction_rows_dedupe_redact_and_relabel() {
    use sun_core::model::{Correction, CorrectionCause, CorrectionKind, SecretScope};
    use sun_core::{CorrectionId, EcosystemId};
    use sun_store::{CorrectionRepo, SecretRepo};

    let f = fixture().await;
    let task_id = TaskId::generate();
    let at = chrono::Utc::now();
    let correction = |body: &str, at| Correction {
        id: CorrectionId::generate(),
        task_id,
        run_id: f.run_id,
        workflow_id: WorkflowId::generate(),
        step_key: Some(key("review")),
        runtime: Some("claude-code".into()),
        kind: CorrectionKind::Reject,
        cause_label: CorrectionCause::Other,
        classified_by: "rules".into(),
        body: body.into(),
        created_at: at,
    };
    let first = correction("leaked hunter2-secret-value here", at);
    let later = correction("second", at + chrono::Duration::seconds(1));
    let mut tx = f.store.begin().await.unwrap();
    let ws = SecretScope::Ecosystem(EcosystemId::generate());
    tx.set_secret(&ws, "TOKEN", "hunter2-secret-value")
        .await
        .unwrap();
    assert!(
        tx.insert_correction(&first, Some("revert:abc"))
            .await
            .unwrap()
    );
    // A redelivery with the same key is a no-op; no key never collides.
    assert!(
        !tx.insert_correction(&later, Some("revert:abc"))
            .await
            .unwrap()
    );
    assert!(tx.insert_correction(&later, None).await.unwrap());
    tx.set_correction_cause(first.id, CorrectionCause::WrongScope, "laya-1")
        .await
        .unwrap();
    assert!(
        tx.set_correction_cause(CorrectionId::generate(), CorrectionCause::Lint, "x")
            .await
            .is_err()
    );
    tx.commit().await.unwrap();

    let mut tx = f.store.read().await.unwrap();
    let rows = tx.corrections_of(task_id).await.unwrap();
    assert_eq!(rows.len(), 2);
    assert!(!rows[0].body.contains("hunter2"), "{}", rows[0].body);
    assert_eq!(rows[0].cause_label, CorrectionCause::WrongScope);
    assert_eq!(rows[0].classified_by, "laya-1");
    assert_eq!(rows[1], later);
    let newest: Vec<CorrectionId> = tx
        .corrections(1)
        .await
        .unwrap()
        .iter()
        .map(|c| c.id)
        .collect();
    assert_eq!(newest, [later.id]);
    assert!(
        tx.corrections_of(TaskId::generate())
            .await
            .unwrap()
            .is_empty()
    );
}

/// A caller cancelled in the middle of `BEGIN` (an aborted engine task, a
/// dropped request) must not leave the single writer stuck inside an
/// orphaned transaction (seen as a flaky failover test under load).
#[tokio::test(flavor = "multi_thread")]
async fn a_begin_cancelled_midway_never_poisons_the_writer() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).await.unwrap();
    for i in 0..400u64 {
        let wait = std::time::Duration::from_micros(i % 200);
        let _ = tokio::time::timeout(wait, store.begin()).await;
        let _ = tokio::time::timeout(wait, store.read()).await;
    }
    for _ in 0..3 {
        store.begin().await.unwrap().commit().await.unwrap();
        drop(store.read().await.unwrap());
    }
}

#[tokio::test]
async fn an_id_is_used_once_until_it_expires() {
    use sun_store::SettingRepo;
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).await.unwrap();
    let now = chrono::Utc::now();
    let later = now + chrono::Duration::minutes(10);
    let mut tx = store.begin().await.unwrap();
    assert!(tx.use_once("delivery:a", later).await.unwrap());
    assert!(!tx.use_once("delivery:a", later).await.unwrap(), "a replay");
    assert!(tx.use_once("delivery:b", later).await.unwrap());
    // A released id (its use failed) is fresh again.
    tx.release_once("delivery:a").await.unwrap();
    assert!(tx.use_once("delivery:a", later).await.unwrap());
    // An expired use is pruned, so the id is fresh again.
    let past = now - chrono::Duration::seconds(5);
    assert!(tx.use_once("delivery:c", past).await.unwrap());
    assert!(tx.use_once("delivery:c", later).await.unwrap());
    tx.commit().await.unwrap();
}
