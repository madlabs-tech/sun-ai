//! Ecosystem export → fresh data dir → import round trip.
#![allow(clippy::unwrap_used)]

use serde_json::json;
use sun_core::model::{Budget, Run, SecretScope, Step, Task, Workflow};
use sun_core::{EcosystemId, RunId, RunStatus, StepKey, StepStatus, TaskStatus, WorkflowId};
use sun_store::{
    ArchiveSecrets, EventLog, MASTER_KEY_FILE, NewEvent, RunRepo, SecretRepo, Store, StoreError,
    TaskRepo, WorkflowRepo,
};

const SECRET: &str = "sk-live-export-secret-value";

/// A store with a workflow (v1), task, run, two steps, events, blobs and a secret.
async fn seeded() -> (tempfile::TempDir, Store, RunId) {
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
        budget: Budget::default(),
    };
    let transcript = store.blobs().put(b"transcript ".repeat(500)).await.unwrap();
    store.blobs().put(b"diff".to_vec()).await.unwrap();

    let mut tx = store.begin().await.unwrap();
    tx.insert_workflow(&workflow).await.unwrap();
    tx.save_workflow_version(workflow.id, "name: default", "h")
        .await
        .unwrap();
    tx.insert_task(&task).await.unwrap();
    tx.insert_run(&run).await.unwrap();
    for key in ["plan", "implement"] {
        tx.insert_step(&Step {
            id: sun_core::StepId::generate(),
            run_id: run.id,
            step_key: StepKey::new(key).unwrap(),
            attempt: 1,
            status: StepStatus::Queued,
            runtime_id: None,
            session_id: None,
            started_at: None,
            ended_at: None,
        })
        .await
        .unwrap();
    }
    let scope = SecretScope::Ecosystem(EcosystemId::generate());
    tx.set_secret(&scope, "API_KEY", SECRET).await.unwrap();
    for n in 0..3 {
        tx.append_event(NewEvent::new(
            run.id,
            None,
            "note",
            json!({ "n": n, "blob": transcript.as_str(), "leak": SECRET }),
        ))
        .await
        .unwrap();
    }
    tx.commit().await.unwrap();
    (dir, store, run.id)
}

/// Everything a run is made of, read back through the repositories.
async fn snapshot(
    store: &Store,
    run_id: RunId,
) -> (
    Run,
    Vec<Step>,
    Vec<sun_core::model::EventRecord>,
    Vec<Vec<u8>>,
) {
    let mut tx = store.read().await.unwrap();
    let run = tx.get_run(run_id).await.unwrap();
    let steps = tx.steps_of_run(run_id).await.unwrap();
    let events = tx.events_after(Some(run_id), 0, 100).await.unwrap();
    let workflow = tx.get_workflow(run.workflow_id).await.unwrap();
    assert_eq!(workflow.name, "default");
    let mut blobs = Vec::new();
    for event in &events {
        let hash = sun_core::BlobHash::new(event.payload["blob"].as_str().unwrap()).unwrap();
        blobs.push(store.blobs().get(&hash).await.unwrap());
    }
    (run, steps, events, blobs)
}

#[tokio::test]
async fn export_import_round_trips_runs_events_blobs() {
    let (_src, store, run_id) = seeded().await;
    let out = tempfile::tempdir().unwrap();
    let archive = out.path().join("ecosystem.sunpack");
    store
        .export(&archive, ArchiveSecrets::WithKey)
        .await
        .unwrap();

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&archive).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }
    // Single file, secret sealed (the event payload was redacted on append).
    let bytes = std::fs::read(&archive).unwrap();
    assert!(!bytes.windows(SECRET.len()).any(|w| w == SECRET.as_bytes()));

    let fresh = tempfile::tempdir().unwrap();
    let imported = Store::import(&archive, fresh.path()).await.unwrap();
    assert_eq!(
        snapshot(&imported, run_id).await,
        snapshot(&store, run_id).await
    );
    assert_eq!(snapshot(&imported, run_id).await.2.len(), 3);
    assert!(!fresh.path().join("sun.db.import").exists());

    // Every blob file came across, not just the referenced ones.
    let diff = sun_core::BlobHash::new(format!(
        "{:x}",
        <sha2::Sha256 as sha2::Digest>::digest(b"diff")
    ))
    .unwrap();
    assert_eq!(imported.blobs().get(&diff).await.unwrap(), b"diff");

    // The key came along, so secrets open; the stores stay independent.
    let mut tx = imported.read().await.unwrap();
    assert_eq!(tx.known_secrets().await.unwrap(), vec![SECRET.to_owned()]);
    drop(tx);

    // A second import into a used data dir is refused.
    let again = Store::import(&archive, fresh.path()).await;
    assert!(matches!(
        again,
        Err(StoreError::Core(sun_core::Error::InvalidInput(_)))
    ));
}

#[tokio::test]
async fn export_without_master_key_needs_the_key_to_import() {
    let (src, store, run_id) = seeded().await;
    let out = tempfile::tempdir().unwrap();
    let archive = out.path().join("ecosystem.sunpack");
    store
        .export(&archive, ArchiveSecrets::Sealed)
        .await
        .unwrap();
    let key = std::fs::read_to_string(src.path().join(MASTER_KEY_FILE)).unwrap();
    let bytes = std::fs::read(&archive).unwrap();
    assert!(!bytes.windows(64).any(|w| w == key.trim().as_bytes()));

    // No key anywhere: refused, nothing written.
    let fresh = tempfile::tempdir().unwrap();
    let refused = Store::import(&archive, fresh.path()).await;
    assert!(matches!(refused, Err(StoreError::Crypto(_))), "{refused:?}");
    assert_eq!(std::fs::read_dir(fresh.path()).unwrap().count(), 0);

    // A wrong key is refused before anything is written too.
    let wrong = tempfile::tempdir().unwrap();
    Store::open(wrong.path()).await.unwrap();
    let other = tempfile::tempdir().unwrap();
    std::fs::copy(
        wrong.path().join(MASTER_KEY_FILE),
        other.path().join(MASTER_KEY_FILE),
    )
    .unwrap();
    let refused = Store::import(&archive, other.path()).await;
    assert!(matches!(refused, Err(StoreError::Crypto(_))), "{refused:?}");
    assert!(!other.path().join("sun.db").exists());

    // The source key copied in first: imports, and secrets open.
    let keyed = tempfile::tempdir().unwrap();
    std::fs::copy(
        src.path().join(MASTER_KEY_FILE),
        keyed.path().join(MASTER_KEY_FILE),
    )
    .unwrap();
    let imported = Store::import(&archive, keyed.path()).await.unwrap();
    assert_eq!(
        snapshot(&imported, run_id).await,
        snapshot(&store, run_id).await
    );
    let mut tx = imported.read().await.unwrap();
    assert_eq!(tx.known_secrets().await.unwrap(), vec![SECRET.to_owned()]);
}

/// `--drop-secrets` hands the copy to the importer's key: the canary is the
/// new key's, so the source key no longer opens the copy.
#[tokio::test]
async fn dropped_export_takes_the_importers_key_and_forgets_the_old_one() {
    let (src, store, run_id) = seeded().await;
    let out = tempfile::tempdir().unwrap();
    let archive = out.path().join("dropped.sunpack");
    store
        .export(&archive, ArchiveSecrets::Dropped)
        .await
        .unwrap();

    // A data dir that already has its own, different key.
    let other = tempfile::tempdir().unwrap();
    Store::open(other.path()).await.unwrap();
    let fresh = tempfile::tempdir().unwrap();
    std::fs::copy(
        other.path().join(MASTER_KEY_FILE),
        fresh.path().join(MASTER_KEY_FILE),
    )
    .unwrap();
    let imported = Store::import(&archive, fresh.path()).await.unwrap();
    assert_eq!(
        snapshot(&imported, run_id).await,
        snapshot(&store, run_id).await
    );
    drop(imported);
    // The new key opens it again...
    Store::open(fresh.path()).await.unwrap();
    // ...and the source's key is refused by the copy's own canary.
    std::fs::copy(
        src.path().join(MASTER_KEY_FILE),
        fresh.path().join(MASTER_KEY_FILE),
    )
    .unwrap();
    let refused = Store::open(fresh.path()).await;
    assert!(matches!(refused, Err(StoreError::Crypto(_))), "{refused:?}");
}

#[tokio::test]
async fn export_import_rejects_non_archives_and_tampered_blobs() {
    let out = tempfile::tempdir().unwrap();
    // A plain sun.db is not an archive.
    let (src, store, _) = seeded().await;
    let plain = out.path().join("plain.db");
    store.backup(&plain).await.unwrap();
    let fresh = tempfile::tempdir().unwrap();
    let refused = Store::import(&plain, fresh.path()).await;
    assert!(matches!(
        refused,
        Err(StoreError::Core(sun_core::Error::InvalidInput(_)))
    ));
    assert!(!fresh.path().join("sun.db").exists());

    // Export refuses to overwrite.
    assert!(store.export(&plain, ArchiveSecrets::Sealed).await.is_err());

    // A blob whose bytes don't match its hash is refused.
    let archive = out.path().join("w.sunpack");
    store
        .export(&archive, ArchiveSecrets::WithKey)
        .await
        .unwrap();
    let conn = sqlx::SqlitePool::connect(&format!("sqlite://{}", archive.display()))
        .await
        .unwrap();
    sqlx::query("UPDATE sun_export_blob SET zst = ?")
        .bind(zstd::encode_all(&b"tampered"[..], 3).unwrap())
        .execute(&conn)
        .await
        .unwrap();
    conn.close().await;
    let fresh = tempfile::tempdir().unwrap();
    let refused = Store::import(&archive, fresh.path()).await;
    assert!(
        matches!(refused, Err(StoreError::CorruptBlob(_))),
        "{refused:?}"
    );
    assert!(!fresh.path().join("sun.db").exists());
    drop(src);
}

#[tokio::test]
async fn export_round_trip_keeps_mods_and_skills() {
    let (src, store, run_id) = seeded().await;
    let data = src.path();
    let write = |path: &str, text: &str| {
        let path = data.join(path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    };
    write("sun.yaml", "listen: 127.0.0.1:7777\n");
    write("mods/market/mod.yaml", "name: market\n");
    write("mods/market/run", "#!/bin/sh\n");
    write("mod-data/market/state.json", "{}");
    write("skills/review/SKILL.md", "# Review\n");
    write("mcp_servers/linear.json", "{\"name\":\"linear\"}");
    write("catalog/installed.json", "[\"review\"]");
    write("license", "sunlic1.not-carried");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let run = data.join("mods/market/run");
        std::fs::set_permissions(&run, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::os::unix::fs::symlink("/etc/passwd", data.join("mods/market/link")).unwrap();
    }

    let out = tempfile::tempdir().unwrap();
    let archive = out.path().join("w.sunpack");
    store
        .export(&archive, ArchiveSecrets::Dropped)
        .await
        .unwrap();
    // No sealed value and no key left the ecosystem.
    let conn = sqlx::SqlitePool::connect(&format!("sqlite://{}", archive.display()))
        .await
        .unwrap();
    let sealed: i64 = sqlx::query_scalar("SELECT count(*) FROM secret")
        .fetch_one(&conn)
        .await
        .unwrap();
    assert_eq!(sealed, 0);
    conn.close().await;

    let fresh = tempfile::tempdir().unwrap();
    let imported = Store::import(&archive, fresh.path()).await.unwrap();
    assert_eq!(
        snapshot(&imported, run_id).await,
        snapshot(&store, run_id).await
    );
    let read = |path: &str| std::fs::read_to_string(fresh.path().join(path)).unwrap();
    assert_eq!(read("sun.yaml"), "listen: 127.0.0.1:7777\n");
    assert_eq!(read("mods/market/mod.yaml"), "name: market\n");
    assert_eq!(read("mod-data/market/state.json"), "{}");
    assert_eq!(read("skills/review/SKILL.md"), "# Review\n");
    assert_eq!(read("mcp_servers/linear.json"), "{\"name\":\"linear\"}");
    assert_eq!(read("catalog/installed.json"), "[\"review\"]");
    assert!(!fresh.path().join("license").exists());
    assert!(!fresh.path().join("mods/market/link").exists());
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(fresh.path().join("mods/market/run"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o755);
    }

    // The dropped secret waits for re-entry, and leaves the list once set.
    let mut tx = imported.begin().await.unwrap();
    let pending = tx.secrets_to_reenter().await.unwrap();
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].name, "API_KEY");
    assert!(tx.known_secrets().await.unwrap().is_empty());
    tx.set_secret(&pending[0].scope, "API_KEY", "a-new-value-123")
        .await
        .unwrap();
    assert!(tx.secrets_to_reenter().await.unwrap().is_empty());
    tx.commit().await.unwrap();

    // An archive path that escapes the data dir is refused before writing.
    let evil = out.path().join("evil.sunpack");
    store.export(&evil, ArchiveSecrets::Dropped).await.unwrap();
    let conn = sqlx::SqlitePool::connect(&format!("sqlite://{}", evil.display()))
        .await
        .unwrap();
    sqlx::query("UPDATE sun_export_file SET path = 'mods/../../escape' WHERE path = 'sun.yaml'")
        .execute(&conn)
        .await
        .unwrap();
    conn.close().await;
    let target = tempfile::tempdir().unwrap();
    let refused = Store::import(&evil, target.path().join("data")).await;
    assert!(
        matches!(
            refused,
            Err(StoreError::Core(sun_core::Error::InvalidInput(_)))
        ),
        "{refused:?}"
    );
    assert!(!target.path().join("escape").exists());
    assert!(!target.path().join("data/sun.db").exists());
}
