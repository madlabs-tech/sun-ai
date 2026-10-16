//! Tasks: the T23 rebuild on a seeded old database, numbers, keys, chats
//! and the activity log.
#![allow(clippy::unwrap_used)]

use serde_json::json;
use sqlx::migrate::Migrate;
use sqlx::{Connection, SqliteConnection};
use sun_core::model::{ActivityKind, Priority, Run, Task, TaskKey, TaskKind};
use sun_core::{RunId, RunStatus, TaskId, TaskStatus, WorkflowId, WorkspaceId};
use sun_store::{RunRepo, Store, TaskRepo, WorkspaceRepo};

const T23: i64 = 20261022090000;
const OPS: &str = "01K0000000000000000000000B";

/// Old task rows: id, workflow (`main`/`ops`), status, priority, and the
/// status and number the migration must give.
const OLD: [(&str, &str, &str, i64, TaskStatus, u32); 11] = [
    (
        "01K00000010000000000000000",
        "main",
        "backlog",
        0,
        TaskStatus::Backlog,
        1,
    ),
    (
        "01K00000020000000000000000",
        "main",
        "queued",
        7,
        TaskStatus::Todo,
        2,
    ),
    // Its latest steps: one running, one waiting approval.
    (
        "01K00000030000000000000000",
        "main",
        "running",
        0,
        TaskStatus::NeedsYou,
        3,
    ),
    // Waited for approval on attempt 1; attempt 2 runs.
    (
        "01K00000040000000000000000",
        "main",
        "running",
        0,
        TaskStatus::InProgress,
        4,
    ),
    (
        "01K00000050000000000000000",
        "main",
        "running",
        0,
        TaskStatus::Verifying,
        5,
    ),
    // Needed attention with a paused run, then with a running one.
    (
        "01K00000060000000000000000",
        "main",
        "needs_attention",
        0,
        TaskStatus::Blocked,
        6,
    ),
    (
        "01K00000070000000000000000",
        "main",
        "needs_attention",
        0,
        TaskStatus::NeedsYou,
        7,
    ),
    (
        "01K00000080000000000000000",
        "main",
        "done",
        -2,
        TaskStatus::Done,
        8,
    ),
    (
        "01K00000090000000000000000",
        "main",
        "failed",
        0,
        TaskStatus::Failed,
        9,
    ),
    (
        "01K000000A0000000000000000",
        "main",
        "cancelled",
        0,
        TaskStatus::Cancelled,
        10,
    ),
    // Running without a run; the first task of the other workspace.
    (
        "01K000000B0000000000000000",
        "ops",
        "running",
        0,
        TaskStatus::InProgress,
        1,
    ),
];

/// `(task index, run status, [(step key, attempt, status)])`.
/// `(step key, attempt, status)`.
type Steps = &'static [(&'static str, i64, &'static str)];

const RUNS: [(usize, &str, Steps); 5] = [
    (
        2,
        "running",
        &[
            ("implement", 1, "running"),
            ("review", 1, "waiting_approval"),
        ],
    ),
    (
        3,
        "running",
        &[("review", 1, "waiting_approval"), ("review", 2, "running")],
    ),
    (4, "running", &[("implement", 1, "verifying")]),
    (5, "paused", &[("implement", 1, "interrupted")]),
    (6, "running", &[("implement", 1, "running")]),
];

async fn exec(conn: &mut SqliteConnection, sql: &str, binds: &[&str]) {
    let mut query = sqlx::query(sql);
    for bind in binds {
        query = query.bind(*bind);
    }
    query.execute(&mut *conn).await.unwrap();
}

/// A data dir migrated up to just before T23, holding the rows above (in
/// reverse id order, so numbers can't come from insert order).
async fn seed_old(dir: &std::path::Path) {
    let url = format!("sqlite://{}?mode=rwc", dir.join("sun.db").display());
    let mut conn = SqliteConnection::connect(&url).await.unwrap();
    conn.ensure_migrations_table().await.unwrap();
    for migration in sqlx::migrate!("./migrations")
        .iter()
        .filter(|m| m.version < T23)
    {
        conn.apply(migration).await.unwrap();
    }
    exec(&mut conn, "PRAGMA foreign_keys = ON", &[]).await;
    // A second workspace whose prefix clashes with Main's but for case.
    exec(
        &mut conn,
        "INSERT INTO workspace (id, name, slug, issue_prefix, created_at)
         VALUES ($1, 'Ops', 'ops', 'main', '2026-10-01T00:00:00+00:00')",
        &[OPS],
    )
    .await;
    let main_flow = WorkflowId::generate().to_string();
    let ops_flow = WorkflowId::generate().to_string();
    for (flow, workspace) in [(&main_flow, "00000000000000000000000001"), (&ops_flow, OPS)] {
        exec(
            &mut conn,
            "INSERT INTO workflow (id, name, workspace_id) VALUES ($1, 'f', $2)",
            &[flow, workspace],
        )
        .await;
        exec(
            &mut conn,
            "INSERT INTO workflow_version (workflow_id, version, source_yaml, schema_hash)
             VALUES ($1, 1, 'name: f', 'h')",
            &[flow],
        )
        .await;
    }
    for (id, flow, status, priority, ..) in OLD.iter().rev() {
        let flow = if *flow == "main" {
            &main_flow
        } else {
            &ops_flow
        };
        sqlx::query(
            "INSERT INTO task (id, title, body, workflow_id, priority, status)
             VALUES ($1, 'old', '', $2, $3, $4)",
        )
        .bind(id)
        .bind(flow)
        .bind(priority)
        .bind(status)
        .execute(&mut conn)
        .await
        .unwrap();
    }
    for (index, status, steps) in RUNS {
        let run = RunId::generate().to_string();
        exec(
            &mut conn,
            "INSERT INTO run (id, task_id, workflow_id, workflow_version, status)
             VALUES ($1, $2, $3, 1, $4)",
            &[&run, OLD[index].0, &main_flow, status],
        )
        .await;
        for (key, attempt, status) in steps {
            sqlx::query(
                "INSERT INTO step (id, run_id, step_key, attempt, status) VALUES ($1, $2, $3, $4, $5)",
            )
            .bind(RunId::generate().to_string())
            .bind(&run)
            .bind(key)
            .bind(attempt)
            .bind(status)
            .execute(&mut conn)
            .await
            .unwrap();
        }
    }
    // Rows that reference tasks: the rebuild must keep them (task_context
    // would cascade on a delete).
    exec(
        &mut conn,
        "INSERT INTO task_context (task_id) VALUES ($1)",
        &[OLD[2].0],
    )
    .await;
    exec(
        &mut conn,
        "INSERT INTO artifact (id, task_id, version, blob, mime) VALUES ($1, $2, 1, $3, 'text/plain')",
        &[&RunId::generate().to_string(), OLD[7].0, &"a".repeat(64)],
    )
    .await;
    conn.close().await.unwrap();
}

#[tokio::test]
async fn task_migration_backfills_a_seeded_old_database() {
    let dir = tempfile::tempdir().unwrap();
    seed_old(dir.path()).await;
    let store = Store::open(dir.path()).await.unwrap();
    assert!(dir.path().join("backups").exists(), "backed up first");
    let mut tx = store.read().await.unwrap();
    let ops: WorkspaceId = OPS.parse().unwrap();
    for (id, _, _, priority, status, number) in OLD {
        let task = tx.get_task(id.parse().unwrap()).await.unwrap();
        assert_eq!((task.status, task.number), (status, Some(number)), "{id}");
        assert_eq!(task.priority, Priority::from_int(priority as i32), "{id}");
        assert_eq!(task.position, f64::from(number));
        let want = if id == OLD[10].0 {
            ops
        } else {
            WorkspaceId::MAIN
        };
        assert_eq!(task.workspace_id, want, "the workflow's workspace");
        let made = task.id.0.timestamp_ms() as i64;
        assert_eq!(task.created_at.timestamp_millis(), made, "the ULID's time");
        assert_eq!(task.updated_at, task.created_at);
        assert!(tx.activity_of(task.id, 0).await.unwrap().is_empty());
    }
    // Counters continue after the backfilled numbers; prefixes are unique.
    let main = tx.get_workspace(WorkspaceId::MAIN).await.unwrap();
    let other = tx.get_workspace(ops).await.unwrap();
    assert_eq!((main.issue_counter, other.issue_counter), (10, 1));
    assert_eq!(
        (main.issue_prefix.as_str(), other.issue_prefix.as_str()),
        ("MAIN", "MAIN2")
    );
    let key: TaskKey = "main2-1".parse().unwrap();
    assert_eq!(
        tx.task_by_key(&key).await.unwrap().id,
        OLD[10].0.parse().unwrap()
    );
    drop(tx);

    // The rows that reference tasks survived, and foreign keys hold and are on.
    let url = format!("sqlite://{}", dir.path().join("sun.db").display());
    let mut conn = SqliteConnection::connect(&url).await.unwrap();
    let kept: (i64, i64) = sqlx::query_as(
        "SELECT (SELECT count(*) FROM task_context), (SELECT count(*) FROM artifact)",
    )
    .fetch_one(&mut conn)
    .await
    .unwrap();
    assert_eq!(kept, (1, 1));
    let broken: Vec<(String,)> = sqlx::query_as("SELECT \"table\" FROM pragma_foreign_key_check")
        .fetch_all(&mut conn)
        .await
        .unwrap();
    assert!(broken.is_empty(), "{broken:?}");
    conn.close().await.unwrap();
    let mut tx = store.begin().await.unwrap();
    let orphan = Run {
        id: RunId::generate(),
        task_id: TaskId::generate(),
        workflow_id: tx
            .get_task(OLD[0].0.parse().unwrap())
            .await
            .unwrap()
            .workflow_id
            .unwrap(),
        workflow_version: 1,
        status: RunStatus::Queued,
        end_reason: None,
        repo_ids: Vec::new(),
        budget: Default::default(),
    };
    assert!(
        tx.insert_run(&orphan).await.is_err(),
        "foreign keys are back on"
    );
    drop(tx);
    drop(store);
    // Opening again applies nothing twice.
    Store::open(dir.path()).await.unwrap();
}

#[tokio::test]
async fn a_chat_takes_no_number_and_stays_off_task_lists() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).await.unwrap();
    let mut tx = store.begin().await.unwrap();
    let task = Task::new(WorkspaceId::MAIN, "Fix the login");
    assert_eq!(tx.insert_task(&task).await.unwrap(), Some(1));
    let chat = Task {
        kind: TaskKind::Chat,
        source_task_id: Some(task.id),
        ..Task::new(WorkspaceId::MAIN, "Fix the login")
    };
    assert_eq!(tx.insert_task(&chat).await.unwrap(), None);
    let stored = tx.get_task(chat.id).await.unwrap();
    assert_eq!(
        (stored.number, stored.kind, stored.source_task_id),
        (None, TaskKind::Chat, Some(task.id))
    );
    let ids = |tasks: Vec<Task>| tasks.into_iter().map(|t| t.id).collect::<Vec<_>>();
    assert_eq!(ids(tx.tasks().await.unwrap()), [task.id]);
    let on_main = tx.tasks_of_workspace(WorkspaceId::MAIN).await.unwrap();
    assert_eq!(ids(on_main), [task.id]);
    // No gap in the keys: the next task is 2.
    let next = Task::new(WorkspaceId::MAIN, "Next");
    assert_eq!(tx.insert_task(&next).await.unwrap(), Some(2));
}

#[tokio::test]
async fn task_numbers_are_unique_under_concurrent_creates() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).await.unwrap();
    let creates: Vec<_> = (0..20)
        .map(|i| {
            let store = store.clone();
            tokio::spawn(async move {
                let mut tx = store.begin().await.unwrap();
                let number = tx
                    .insert_task(&Task::new(WorkspaceId::MAIN, format!("t{i}")))
                    .await
                    .unwrap();
                tx.commit().await.unwrap();
                number
            })
        })
        .collect();
    let mut numbers = Vec::new();
    for create in creates {
        numbers.push(create.await.unwrap().unwrap());
    }
    numbers.sort_unstable();
    assert_eq!(numbers, (1..=20).collect::<Vec<u32>>());
    let mut tx = store.read().await.unwrap();
    let main = tx.get_workspace(WorkspaceId::MAIN).await.unwrap();
    assert_eq!(main.issue_counter, 20);
    let key = TaskKey {
        prefix: "main".into(),
        number: 7,
    };
    assert_eq!(tx.task_by_key(&key).await.unwrap().number, Some(7));
    let missing = TaskKey {
        prefix: "MAIN".into(),
        number: 21,
    };
    assert!(tx.task_by_key(&missing).await.is_err());
}

#[tokio::test]
async fn a_status_move_and_its_activity_commit_or_vanish_together() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).await.unwrap();
    let task = Task::new(WorkspaceId::MAIN, "plain");
    let mut tx = store.begin().await.unwrap();
    tx.insert_task(&task).await.unwrap();
    tx.commit().await.unwrap();
    let mut live = store.subscribe_activity();

    // A crash between the two writes is a transaction never committed.
    let mut tx = store.begin().await.unwrap();
    tx.set_task_status(task.id, TaskStatus::InProgress)
        .await
        .unwrap();
    let body = json!({ "from": "todo", "to": "in_progress" });
    tx.append_activity(task.id, ActivityKind::Status, None, body.clone())
        .await
        .unwrap();
    drop(tx);
    let mut tx = store.read().await.unwrap();
    assert_eq!(tx.get_task(task.id).await.unwrap().status, TaskStatus::Todo);
    assert!(tx.activity_of(task.id, 0).await.unwrap().is_empty());
    drop(tx);
    assert!(live.try_recv().is_err(), "nothing rolled back is broadcast");

    // Committed: both, `seq` from 1, then broadcast.
    let mut tx = store.begin().await.unwrap();
    tx.set_task_status(task.id, TaskStatus::InProgress)
        .await
        .unwrap();
    tx.append_activity(task.id, ActivityKind::Status, None, body.clone())
        .await
        .unwrap();
    let comment = json!({ "text": "looks good" });
    tx.append_activity(task.id, ActivityKind::Comment, None, comment)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    let mut tx = store.read().await.unwrap();
    let log = tx.activity_of(task.id, 0).await.unwrap();
    assert_eq!(log.iter().map(|a| a.seq).collect::<Vec<_>>(), [1, 2]);
    assert_eq!((log[0].kind, &log[0].body), (ActivityKind::Status, &body));
    assert_eq!(tx.activity_of(task.id, 1).await.unwrap().len(), 1);
    let saved = tx.get_task(task.id).await.unwrap();
    assert!(saved.updated_at >= saved.created_at);
    assert_eq!(live.try_recv().unwrap().seq, 1);
    // An illegal move writes nothing.
    drop(tx);
    let mut tx = store.begin().await.unwrap();
    assert!(tx.set_task_status(task.id, TaskStatus::Done).await.is_err());
}
