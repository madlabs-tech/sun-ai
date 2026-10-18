//! Runtime cooldowns, session rows, hand-off pack rows and notes (1.12b).
#![allow(clippy::unwrap_used)]

use sun_core::model::{
    Budget, Credential, HandoffPack, Note, NoteKind, Run, RuntimeSource, Step, Task, Workflow,
};
use sun_core::{
    HandoffPackId, HostId, NoteId, RunId, RunStatus, SessionHandle, SessionId, StepId, StepKey,
    StepStatus, TaskStatus, WorkflowId, WorkspaceId,
};
use sun_store::{
    CredentialRepo, NoteRepo, RunRepo, RuntimeRepo, SessionRepo, StepPayer, Store, TaskRepo,
    WorkflowRepo,
};

/// A queued run of a new task with one queued `implement` step.
async fn seeded_run(tx: &mut sun_store::Tx) -> (RunId, Step) {
    let workflow = Workflow {
        id: WorkflowId::generate(),
        workspace_id: sun_core::WorkspaceId::MAIN,
        name: format!("w-{}", WorkflowId::generate()),
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
        credential_id: None,
        session_id: None,
        started_at: None,
        ended_at: None,
    };
    tx.insert_workflow(&workflow).await.unwrap();
    tx.save_workflow_version(workflow.id, "name: default", "h")
        .await
        .unwrap();
    tx.insert_task(&task).await.unwrap();
    tx.insert_run(&run).await.unwrap();
    tx.insert_step(&step).await.unwrap();
    (run.id, step)
}

#[tokio::test]
async fn runtime_session_handoff_and_note_rows_round_trip() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).await.unwrap();
    let mut tx = store.begin().await.unwrap();
    let (run_id, step) = seeded_run(&mut tx).await;

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
    tx.assign_step_session(step.id, StepPayer::Login(configured.id), new)
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
        run_id,
        from_session: old,
        to_session: Some(new),
        pack_hash: hash,
    })
    .await
    .unwrap();

    let note = |body: &str| Note {
        id: NoteId::generate(),
        run_id,
        step_key: step.step_key.clone(),
        kind: NoteKind::Decision,
        body: body.into(),
        superseded_by: None,
    };
    let (first, second) = (note("keep SQLite"), note("no Redis"));
    tx.insert_note(&first).await.unwrap();
    tx.insert_note(&second).await.unwrap();
    assert_eq!(tx.notes_of_run(run_id).await.unwrap(), [first, second]);
    tx.commit().await.unwrap();
}

#[tokio::test]
async fn byo_key_credential_round_trips_and_pays_for_a_step() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).await.unwrap();
    let mut credential = Credential {
        id: sun_core::CredentialId::generate(),
        workspace_id: WorkspaceId::MAIN,
        name: "ollama".into(),
        source: RuntimeSource::ByoKey,
        default_tool: "opencode".into(),
        byo_key: Some(sun_core::ByoKey {
            kind: sun_core::ByoKeyKind::OpenaiCompatible,
            endpoint: "http://localhost:11434/v1".into(),
            model: "qwen2.5-coder:7b".into(),
            key_secret: None,
        }),
        created_by: None,
    };
    let mut tx = store.begin().await.unwrap();
    assert!(tx.insert_credential(&credential).await.unwrap());
    assert!(
        !tx.insert_credential(&credential).await.unwrap(),
        "name taken"
    );
    assert_eq!(
        tx.credential_by_name(WorkspaceId::MAIN, "ollama")
            .await
            .unwrap(),
        Some(credential.clone())
    );

    if let Some(byo) = credential.byo_key.as_mut() {
        byo.key_secret = Some("OLLAMA_KEY".into());
        // The provider's own endpoint reads back empty.
        byo.kind = sun_core::ByoKeyKind::Anthropic;
        byo.endpoint = String::new();
    }
    assert!(tx.update_credential(&credential).await.unwrap());
    assert_eq!(tx.get_credential(credential.id).await.unwrap(), credential);
    // A tool row of the same name is the CLI's login, not the credential.
    tx.upsert_runtime(
        WorkspaceId::MAIN,
        "codex",
        "codex",
        RuntimeSource::ByoRuntime,
    )
    .await
    .unwrap();
    let names: Vec<_> = tx
        .credentials(Some(WorkspaceId::MAIN))
        .await
        .unwrap()
        .into_iter()
        .map(|c| c.name)
        .collect();
    assert_eq!(names, ["ollama"]);
    let mut missing = credential.clone();
    missing.name = "missing".into();
    assert!(!tx.update_credential(&missing).await.unwrap());

    // A step paid by it names it, not a runtime row; deleting the
    // credential keeps the step without the link, and takes its cooldown.
    let (run, step) = seeded_run(&mut tx).await;
    let session = SessionId::generate();
    tx.insert_session(session, HostId::generate())
        .await
        .unwrap();
    tx.assign_step_session(step.id, StepPayer::Credential(credential.id), session)
        .await
        .unwrap();
    let paid = tx.get_step(step.id).await.unwrap();
    assert_eq!(
        (paid.runtime_id, paid.credential_id),
        (None, Some(credential.id))
    );
    let account = format!("cred:{}", credential.id);
    tx.set_cooldown(&account, Some(chrono::Utc::now()))
        .await
        .unwrap();
    let gone = tx.delete_credential(WorkspaceId::MAIN, "ollama").await;
    assert_eq!(gone.unwrap().map(|c| c.id), Some(credential.id));
    assert!(tx.cooldowns().await.unwrap().is_empty());
    let kept = tx.get_step(step.id).await.unwrap();
    assert_eq!((kept.run_id, kept.credential_id), (run, None));
    assert!(
        tx.delete_credential(WorkspaceId::MAIN, "ollama")
            .await
            .unwrap()
            .is_none()
    );
    tx.commit().await.unwrap();
}
