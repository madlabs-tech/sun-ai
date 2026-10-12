//! Secret store tests: sealed at rest, scoped, validated, and
//! never present in the ledger. Every test name contains `secret` so the
//! phase exit command `cargo test -p sun-store secret` runs them all.
#![allow(clippy::unwrap_used)]

use std::path::Path;

use serde_json::json;
use sun_core::model::{Budget, Run, SecretScope, Task, Workflow};
use sun_core::{EcosystemId, RunId, RunStatus, SunEvent, TaskStatus, WorkflowId};
use sun_store::{
    EventLog, MASTER_KEY_FILE, NewEvent, RunRepo, SecretRepo, Store, StoreError, TaskRepo,
    WorkflowRepo,
};

/// Long, and shaped like no known token, so only the known-value pass of
/// `redact` can catch it.
const VALUE: &str = "correct-horse-battery-staple-42";

fn ecosystem() -> SecretScope {
    SecretScope::Ecosystem(EcosystemId::generate())
}

/// Every byte SQLite has written for the DB (main file + WAL).
fn db_bytes(dir: &Path) -> Vec<u8> {
    ["sun.db", "sun.db-wal"]
        .iter()
        .filter_map(|f| std::fs::read(dir.join(f)).ok())
        .flatten()
        .collect()
}

fn contains(haystack: &[u8], needle: &str) -> bool {
    haystack
        .windows(needle.len())
        .any(|w| w == needle.as_bytes())
}

async fn run(store: &Store) -> RunId {
    let workflow = Workflow {
        id: WorkflowId::generate(),
        workspace_id: sun_core::WorkspaceId::MAIN,
        name: "default".into(),
    };
    let task = Task {
        title: "t".into(),
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
    let mut tx = store.begin().await.unwrap();
    tx.insert_workflow(&workflow).await.unwrap();
    tx.save_workflow_version(workflow.id, "name: default", "h")
        .await
        .unwrap();
    tx.insert_task(&task).await.unwrap();
    tx.insert_run(&run).await.unwrap();
    tx.commit().await.unwrap();
    run.id
}

#[tokio::test]
async fn secret_round_trip_is_scoped_and_sealed_at_rest() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).await.unwrap();
    let (ws, slack) = (ecosystem(), SecretScope::Mod("slack".into()));

    let mut tx = store.begin().await.unwrap();
    let first = tx.set_secret(&ws, "TOKEN", "first-value-1").await.unwrap();
    let again = tx.set_secret(&ws, "TOKEN", VALUE).await.unwrap();
    assert_eq!(first.id, again.id, "an update keeps the id");
    tx.set_secret(&slack, "TOKEN", "slack-value-2")
        .await
        .unwrap();
    tx.commit().await.unwrap();

    let mut tx = store.read().await.unwrap();
    assert_eq!(tx.secret_value(&ws, "TOKEN").await.unwrap(), VALUE);
    assert_eq!(
        tx.secret_value(&slack, "TOKEN").await.unwrap(),
        "slack-value-2"
    );
    let other = SecretScope::Mod("github".into());
    assert!(matches!(
        tx.secret_value(&other, "TOKEN").await,
        Err(StoreError::Core(sun_core::Error::NotFound(_)))
    ));
    let listed = tx.list_secrets(Some(&ws)).await.unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(
        (listed[0].scope.clone(), listed[0].name.as_str()),
        (ws.clone(), "TOKEN")
    );
    assert_eq!(tx.list_secrets(None).await.unwrap().len(), 2);
    let mut known = tx.known_secrets().await.unwrap();
    known.sort();
    assert_eq!(known, [VALUE, "slack-value-2"]);
    // Metadata never carries the value, whichever way it is printed.
    let shown = format!("{listed:?}{}", serde_json::to_string(&listed).unwrap());
    assert!(!shown.contains(VALUE));
    drop(tx);

    // Encrypted at rest: neither the current nor the replaced value is on disk.
    let bytes = db_bytes(dir.path());
    assert!(!bytes.is_empty());
    for plain in [VALUE, "first-value-1", "slack-value-2"] {
        assert!(!contains(&bytes, plain), "{plain} stored in clear");
    }

    let mut tx = store.begin().await.unwrap();
    assert!(tx.delete_secret(&ws, "TOKEN").await.unwrap());
    assert!(!tx.delete_secret(&ws, "TOKEN").await.unwrap());
    tx.commit().await.unwrap();
    let mut tx = store.read().await.unwrap();
    assert_eq!(tx.known_secrets().await.unwrap(), ["slack-value-2"]);
}

#[tokio::test]
async fn secret_input_is_validated_without_echoing_the_value() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).await.unwrap();
    let ws = ecosystem();
    let mut tx = store.begin().await.unwrap();
    // Too short for `redact` to hide, too big, bad names, empty mod name.
    let huge = "x".repeat(sun_store::MAX_SECRET_BYTES + 1);
    for (scope, name, value) in [
        (ws.clone(), "TOKEN", "abc12"),
        (ws.clone(), "TOKEN", huge.as_str()),
        (ws.clone(), "", VALUE),
        (ws.clone(), "1TOKEN", VALUE),
        (ws.clone(), "MY-TOKEN", VALUE),
        (SecretScope::Mod(" ".into()), "TOKEN", VALUE),
    ] {
        let err = tx.set_secret(&scope, name, value).await.unwrap_err();
        assert!(matches!(
            err,
            StoreError::Core(sun_core::Error::InvalidInput(_))
        ));
        let message = err.to_string();
        assert!(
            !message.contains(VALUE) && !message.contains("abc12"),
            "{message}"
        );
    }
    assert!(tx.list_secrets(None).await.unwrap().is_empty());
}

#[tokio::test]
async fn secret_values_never_appear_in_events() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).await.unwrap();
    let run_id = run(&store).await;
    let mut tx = store.begin().await.unwrap();
    tx.set_secret(&ecosystem(), "JIRA_TOKEN", VALUE)
        .await
        .unwrap();
    tx.commit().await.unwrap();

    let mut live = store.subscribe();
    let mut tx = store.begin().await.unwrap();
    // A producer that forgot to redact: nested, in arrays, inside other text.
    let returned = tx
        .append_event(NewEvent::new(
            run_id,
            None,
            "tool_call",
            json!({ "args": { "cmd": format!("curl -H 'X: {VALUE}'"), "list": [VALUE, 1] } }),
        ))
        .await
        .unwrap();
    let agent = SunEvent::Text {
        role: sun_core::Role::Assistant,
        text: format!("the token is {VALUE}."),
    };
    tx.append_event(
        NewEvent::agent(run_id, sun_core::StepKey::new("implement").unwrap(), &agent).unwrap(),
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();

    let broadcast = live.recv().await.unwrap();
    let mut tx = store.read().await.unwrap();
    let stored = tx.events_after(Some(run_id), 0, 10).await.unwrap();
    assert_eq!(stored.len(), 2);
    assert_eq!(stored[0].payload["args"]["cmd"], "curl -H 'X: [REDACTED]'");
    assert_eq!(stored[1].payload["text"], "the token is [REDACTED].");
    let all = format!("{returned:?}{broadcast:?}{stored:?}");
    assert!(!all.contains(VALUE));
    drop(tx);
    assert!(!contains(&db_bytes(dir.path()), VALUE));
}

#[cfg(unix)]
#[tokio::test]
async fn secret_master_key_file_is_0600_and_survives_reopen() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let ws = ecosystem();
    {
        let store = Store::open(dir.path()).await.unwrap();
        let mut tx = store.begin().await.unwrap();
        tx.set_secret(&ws, "TOKEN", VALUE).await.unwrap();
        tx.commit().await.unwrap();
    }
    let key_file = dir.path().join(MASTER_KEY_FILE);
    let mode = std::fs::metadata(&key_file).unwrap().permissions().mode();
    assert_eq!(mode & 0o777, 0o600);
    assert_eq!(std::fs::read_to_string(&key_file).unwrap().trim().len(), 64);

    let store = Store::open(dir.path()).await.unwrap();
    let mut tx = store.read().await.unwrap();
    assert_eq!(tx.secret_value(&ws, "TOKEN").await.unwrap(), VALUE);
}

#[cfg(unix)]
#[tokio::test]
async fn secret_store_refuses_a_wrong_or_exposed_master_key() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    {
        let store = Store::open(dir.path()).await.unwrap();
        let mut tx = store.begin().await.unwrap();
        tx.set_secret(&ecosystem(), "TOKEN", VALUE).await.unwrap();
        tx.commit().await.unwrap();
    }
    let key_file = dir.path().join(MASTER_KEY_FILE);
    let original = std::fs::read_to_string(&key_file).unwrap();

    // Readable by others: refused before anything is decrypted.
    std::fs::set_permissions(&key_file, std::fs::Permissions::from_mode(0o644)).unwrap();
    let err = Store::open(dir.path()).await.unwrap_err();
    assert!(matches!(err, StoreError::Crypto(_)), "{err}");
    assert!(err.to_string().contains("chmod 600"));

    // Another key: the stored secret no longer opens, and the error says so
    // without the value or either key.
    std::fs::set_permissions(&key_file, std::fs::Permissions::from_mode(0o600)).unwrap();
    let wrong = "ab".repeat(32);
    std::fs::write(&key_file, &wrong).unwrap();
    let err = Store::open(dir.path()).await.unwrap_err();
    let message = err.to_string();
    assert!(matches!(err, StoreError::Crypto(_)), "{message}");
    for leak in [VALUE, wrong.as_str(), original.trim()] {
        assert!(!message.contains(leak), "{message}");
    }
}

/// HA standby with the wrong `SUN_MASTER_KEY`: members' authenticator
/// secrets are sealed with the master key too, so a store whose only sealed
/// values are 2-step factors must also refuse a wrong key at open, instead
/// of starting and failing every 2-step sign-in later.
#[tokio::test]
async fn secret_store_refuses_a_wrong_key_that_only_2_step_factors_would_reveal() {
    use sun_core::model::FactorKind;
    use sun_store::FactorRepo;
    let dir = tempfile::tempdir().unwrap();
    let member = sun_core::MemberId::generate();
    {
        let store = Store::open(dir.path()).await.unwrap();
        let mut tx = store.begin().await.unwrap();
        tx.insert_factor(member, FactorKind::Totp, "Phone", Some("00ff"), "")
            .await
            .unwrap();
        tx.commit().await.unwrap();
    }
    std::fs::write(dir.path().join(MASTER_KEY_FILE), "ab".repeat(32)).unwrap();
    let err = Store::open(dir.path()).await.unwrap_err();
    assert!(matches!(err, StoreError::Crypto(_)), "{err}");
}

/// An empty store (no secret, no factor) still refuses a wrong key: the
/// canary sealed at first open tells.
#[tokio::test]
async fn secret_store_refuses_a_wrong_key_with_nothing_sealed() {
    let dir = tempfile::tempdir().unwrap();
    Store::open(dir.path()).await.unwrap();
    std::fs::write(dir.path().join(MASTER_KEY_FILE), "ab".repeat(32)).unwrap();
    let err = Store::open(dir.path()).await.unwrap_err();
    assert!(matches!(err, StoreError::Crypto(_)), "{err}");
    assert!(err.to_string().contains("first opened with"), "{err}");
}

#[tokio::test]
async fn secret_workspace_scope_seals_redacts_and_goes_with_its_workspace() {
    use sun_core::WorkspaceId;
    use sun_core::model::Workspace;
    use sun_store::WorkspaceRepo;

    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).await.unwrap();
    let workspace = Workspace {
        id: WorkspaceId::generate(),
        name: "Payments".into(),
        slug: "payments".into(),
        description: String::new(),
        context: String::new(),
        logo_hash: None,
        issue_prefix: "PAY".into(),
        issue_counter: 0,
        default_workflow_id: None,
        git_settings: Default::default(),
        created_by: None,
        created_at: chrono::Utc::now(),
    };
    let scope = SecretScope::Workspace(workspace.id);
    let mut tx = store.begin().await.unwrap();
    tx.insert_workspace(&workspace).await.unwrap();
    tx.set_secret(&scope, "VCS_X_TOKEN", VALUE).await.unwrap();
    // An app id or URL is kept with the secrets but is not one: never in
    // `known_secrets` (so never redacted or scanned for), and may be short.
    let eco = ecosystem();
    tx.set_secret(&eco, "GITHUB_APP_ID", "12345").await.unwrap();
    tx.set_secret(&eco, "GITLAB_OAUTH_URL", "https://gitlab.com")
        .await
        .unwrap();
    assert!(tx.set_secret(&eco, "SHORT", "12345").await.is_err());
    tx.commit().await.unwrap();
    let mut tx = store.read().await.unwrap();
    assert_eq!(tx.secret_value(&scope, "VCS_X_TOKEN").await.unwrap(), VALUE);
    assert_eq!(tx.known_secrets().await.unwrap(), vec![VALUE.to_owned()]);
    assert_eq!(tx.list_secrets(Some(&scope)).await.unwrap()[0].scope, scope);
    drop(tx);
    assert!(!contains(&db_bytes(dir.path()), VALUE), "sealed at rest");
    // Deleting the workspace deletes its secrets.
    let mut tx = store.begin().await.unwrap();
    tx.delete_workspace(workspace.id).await.unwrap();
    tx.commit().await.unwrap();
    let mut tx = store.read().await.unwrap();
    assert!(tx.list_secrets(Some(&scope)).await.unwrap().is_empty());
}
