//! Runtime cooldowns, session rows, hand-off pack rows and notes (1.12b).
#![allow(clippy::unwrap_used)]

use sun_core::model::{
    Budget, HandoffPack, Note, NoteKind, Run, RuntimeSource, Step, Task, Workflow,
};
use sun_core::{
    HandoffPackId, HostId, NoteId, RunId, RunStatus, SessionHandle, SessionId, StepId, StepKey,
    StepStatus, TaskStatus, WorkflowId, WorkspaceId,
};
use sun_store::{NoteRepo, RunRepo, RuntimeRepo, SessionRepo, Store, TaskRepo, WorkflowRepo};

#[tokio::test]
async fn runtime_session_handoff_and_note_rows_round_trip() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).await.unwrap();
    let workflow = Workflow {
        id: WorkflowId::generate(),
        workspace_id: sun_core::WorkspaceId::MAIN,
        name: "default".into(),
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
        repo_ids: Vec::new(),
        budget: Budget::default(),
    };
    let step = Step {
        id: StepId::generate(),
        run_id: run.id,
        step_key: StepKey::new("implement").unwrap(),
        attempt: 1,
        status: StepStatus::Queued,
        runtime_id: None,
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

    // A configured runtime keeps its adapter; a chain name alone maps to itself.
    let configured = tx
        .upsert_runtime(
            WorkspaceId::MAIN,
            "claude-max",
            "claude-code",
            RuntimeSource::ByoRuntime,
        )
        .await
        .unwrap();
    let again = tx
        .upsert_runtime(
            WorkspaceId::MAIN,
            "claude-max",
            "claude-max",
            RuntimeSource::ByoRuntime,
        )
        .await
        .unwrap();
    assert_eq!(again, configured);
    assert_eq!(again.adapter, "claude-code");
    assert!(
        tx.runtime_by_name(WorkspaceId::MAIN, "codex")
            .await
            .unwrap()
            .is_none()
    );

    // Cooldowns are keyed by account; a later limit moves the end.
    let account = format!("cred:{}", configured.id);
    let until = chrono::Utc::now() + chrono::Duration::minutes(30);
    tx.set_cooldown(&account, Some(until)).await.unwrap();
    let later = until + chrono::Duration::minutes(5);
    tx.set_cooldown(&account, Some(later)).await.unwrap();
    assert_eq!(tx.cooldowns().await.unwrap(), [(account.clone(), later)]);
    tx.set_cooldown(&account, None).await.unwrap();
    assert!(tx.cooldowns().await.unwrap().is_empty());

    // Sessions need no host row (a revoked host keeps its history).
    let (old, new) = (SessionId::generate(), SessionId::generate());
    for id in [old, new] {
        tx.insert_session(id, HostId::generate()).await.unwrap();
    }
    tx.assign_step_session(step.id, configured.id, new)
        .await
        .unwrap();
    let handle = SessionHandle {
        session_id: new,
        native_id: Some("native-1".into()),
        pid: Some(42),
    };
    tx.set_session_started(&handle, "/data/work/t")
        .await
        .unwrap();
    assert_eq!(tx.get_session(new).await.unwrap(), handle);
    assert_eq!(tx.get_step(step.id).await.unwrap().session_id, Some(new));
    let hash = store.blobs().put(b"{}".to_vec()).await.unwrap();
    tx.insert_handoff_pack(&HandoffPack {
        id: HandoffPackId::generate(),
        run_id: run.id,
        from_session: old,
        to_session: Some(new),
        pack_hash: hash,
    })
    .await
    .unwrap();

    let note = |body: &str| Note {
        id: NoteId::generate(),
        run_id: run.id,
        step_key: step.step_key.clone(),
        kind: NoteKind::Decision,
        body: body.into(),
        superseded_by: None,
    };
    let (first, second) = (note("keep SQLite"), note("no Redis"));
    tx.insert_note(&first).await.unwrap();
    tx.insert_note(&second).await.unwrap();
    assert_eq!(tx.notes_of_run(run.id).await.unwrap(), [first, second]);
    tx.commit().await.unwrap();
}

#[tokio::test]
async fn byo_key_runtime_config_round_trips() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).await.unwrap();
    let mut runtime = sun_core::model::Runtime {
        id: sun_core::RuntimeId::generate(),
        workspace_id: WorkspaceId::MAIN,
        name: "ollama".into(),
        adapter: "opencode".into(),
        source: RuntimeSource::ByoKey,
        byo_key: Some(sun_core::ByoKey {
            kind: sun_core::ByoKeyKind::OpenaiCompatible,
            endpoint: "http://localhost:11434/v1".into(),
            model: "qwen2.5-coder:7b".into(),
            key_secret: None,
        }),
        created_by: None,
    };
    let mut tx = store.begin().await.unwrap();
    assert!(tx.insert_runtime(&runtime).await.unwrap());
    assert!(!tx.insert_runtime(&runtime).await.unwrap(), "name taken");
    assert_eq!(
        tx.runtime_by_name(WorkspaceId::MAIN, "ollama")
            .await
            .unwrap(),
        Some(runtime.clone())
    );

    if let Some(byo) = runtime.byo_key.as_mut() {
        byo.key_secret = Some("OLLAMA_KEY".into());
    }
    assert!(tx.update_runtime(&runtime).await.unwrap());
    assert_eq!(tx.get_runtime(runtime.id).await.unwrap(), runtime);
    // The engine's upsert keeps the configured row.
    let upserted = tx
        .upsert_runtime(
            WorkspaceId::MAIN,
            "ollama",
            "ollama",
            RuntimeSource::ByoRuntime,
        )
        .await
        .unwrap();
    assert_eq!(upserted, runtime);
    tx.upsert_runtime(
        WorkspaceId::MAIN,
        "codex",
        "codex",
        RuntimeSource::ByoRuntime,
    )
    .await
    .unwrap();
    let names: Vec<_> = tx
        .runtimes(Some(WorkspaceId::MAIN))
        .await
        .unwrap()
        .into_iter()
        .map(|r| r.name)
        .collect();
    assert_eq!(names, ["codex", "ollama"]);

    runtime.name = "missing".into();
    assert!(!tx.update_runtime(&runtime).await.unwrap());
    tx.commit().await.unwrap();
}
