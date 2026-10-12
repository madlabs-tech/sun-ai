//! Workspaces: CRUD rules, workflows, tasks and repos by workspace, knowledge
//! links, and the migrations that put everything older on workspace Main.
#![allow(clippy::unwrap_used)]

use sqlx::migrate::Migrate;
use sqlx::{Connection, SqliteConnection};
use sun_core::model::{GitSettings, Repo, Task, Workflow, Workspace};
use sun_core::{Error, RepoId, TaskId, TaskStatus, WorkflowId, WorkspaceId};
use sun_store::{
    KnowledgeRepo, KnowledgeSource, RepoRepo, Store, StoreError, TaskRepo, WorkflowRepo,
    WorkspaceItem, WorkspaceRepo,
};

fn workspace(name: &str) -> Workspace {
    let slug = Workspace::slug_from_name(name);
    Workspace {
        id: WorkspaceId::generate(),
        name: name.into(),
        issue_prefix: Workspace::prefix_from_slug(&slug),
        slug,
        description: String::new(),
        context: String::new(),
        logo_hash: None,
        issue_counter: 0,
        default_workflow_id: None,
        git_settings: GitSettings::default(),
        created_at: chrono::Utc::now(),
    }
}

fn invalid(result: Result<(), StoreError>) -> bool {
    matches!(result, Err(StoreError::Core(Error::InvalidInput(_))))
}

#[tokio::test]
async fn workspace_crud_scopes_workflows_and_tasks() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).await.unwrap();
    let mut tx = store.begin().await.unwrap();

    // A fresh store has Main.
    let workspaces = tx.workspaces().await.unwrap();
    assert_eq!(workspaces.len(), 1);
    assert_eq!(
        (workspaces[0].id, workspaces[0].name.as_str()),
        (WorkspaceId::MAIN, "Main")
    );

    let ops = workspace("Ops");
    tx.insert_workspace(&ops).await.unwrap();
    // Names are unique ignoring case, on insert and rename.
    assert!(invalid(tx.insert_workspace(&workspace("ops")).await));
    let renamed = |name: &str| Workspace {
        name: name.into(),
        ..ops.clone()
    };
    assert!(invalid(tx.update_workspace(&renamed("MAIN")).await));
    tx.update_workspace(&renamed("OPS")).await.unwrap(); // its own name, other case
    assert_eq!(tx.get_workspace(ops.id).await.unwrap().name, "OPS");
    // Slugs are unique; a new one de-duplicates with `-2`.
    let same_slug = Workspace {
        slug: "ops".into(),
        ..workspace("Ops team")
    };
    assert!(invalid(tx.insert_workspace(&same_slug).await));
    assert_eq!(tx.free_slug("ops").await.unwrap(), "ops-2");
    assert_eq!(tx.free_slug("ops-team").await.unwrap(), "ops-team");
    // The profile and git settings round-trip; the slug never changes.
    let profile = Workspace {
        description: "Runs the fleet".into(),
        context: "Use the staging cluster.".into(),
        issue_prefix: "OPS".into(),
        slug: "other".into(),
        git_settings: GitSettings {
            move_on_merge: false,
            ..GitSettings::default()
        },
        ..renamed("OPS")
    };
    tx.update_workspace(&profile).await.unwrap();
    let saved = tx.get_workspace(ops.id).await.unwrap();
    assert_eq!(
        saved,
        Workspace {
            slug: "ops".into(),
            ..profile
        }
    );

    // A workflow on each workspace, and a task on each workflow.
    let mut tasks = Vec::new();
    for workspace_id in [WorkspaceId::MAIN, ops.id] {
        let workflow = Workflow {
            id: WorkflowId::generate(),
            workspace_id,
            name: "default".into(),
        };
        tx.insert_workflow(&workflow).await.unwrap();
        let task = Task {
            workflow_id: Some(workflow.id),
            status: TaskStatus::Backlog,
            ..Task::new(workspace_id, "fix")
        };
        let number = tx.insert_task(&task).await.unwrap();
        assert_eq!(number, 1, "numbers count per workspace");
        let task = Task {
            number,
            position: number.into(),
            ..task
        };
        tasks.push((workflow, task));
    }
    let of_ops = tx.workflows_of_workspace(ops.id).await.unwrap();
    assert_eq!(of_ops, vec![tasks[1].0.clone()]);
    assert_eq!(
        tx.tasks_of_workspace(ops.id).await.unwrap(),
        vec![tasks[1].1.clone()]
    );
    assert_eq!(
        tx.tasks_of_workspace(WorkspaceId::MAIN).await.unwrap(),
        vec![tasks[0].1.clone()]
    );
    assert_eq!(tx.workflows().await.unwrap().len(), 2);
    // A task's workflow is on its own workspace.
    let crossed = Task {
        workflow_id: Some(tasks[1].0.id),
        ..Task::new(WorkspaceId::MAIN, "crossed")
    };
    assert!(matches!(
        tx.insert_task(&crossed).await,
        Err(StoreError::Core(Error::InvalidInput(_)))
    ));
    // Issue prefixes are unique in the ecosystem, ignoring case.
    let taken = Workspace {
        issue_prefix: "ops".into(),
        ..workspace("Elsewhere")
    };
    assert!(invalid(tx.insert_workspace(&taken).await));
    let main = tx.get_workspace(WorkspaceId::MAIN).await.unwrap();
    let clash = Workspace {
        issue_prefix: "OPS".into(),
        ..main.clone()
    };
    assert!(invalid(tx.update_workspace(&clash).await));
    assert_eq!(tx.free_prefix("OPS").await.unwrap(), "OPS2");
    assert_eq!(tx.free_prefix("NEW").await.unwrap(), "NEW");

    // Only an empty workspace can be deleted.
    assert!(invalid(tx.delete_workspace(ops.id).await));
    let empty = workspace("Empty");
    tx.insert_workspace(&empty).await.unwrap();
    tx.delete_workspace(empty.id).await.unwrap();
    assert!(matches!(
        tx.get_workspace(empty.id).await,
        Err(StoreError::Core(Error::NotFound(_)))
    ));
    // A workflow can't point at a missing workspace.
    let orphan = Workflow {
        id: WorkflowId::generate(),
        workspace_id: WorkspaceId::generate(),
        name: "x".into(),
    };
    assert!(tx.insert_workflow(&orphan).await.is_err());
}

#[tokio::test]
async fn workspace_migration_puts_existing_workflows_on_main() {
    let dir = tempfile::tempdir().unwrap();
    let url = format!("sqlite://{}?mode=rwc", dir.path().join("sun.db").display());
    let mut conn = SqliteConnection::connect(&url).await.unwrap();
    conn.ensure_migrations_table().await.unwrap();
    // Every migration before workspaces, then a workflow with a version and a task.
    let migrator = sqlx::migrate!("./migrations");
    for migration in migrator.iter().filter(|m| m.version < 20261011090000) {
        conn.apply(migration).await.unwrap();
    }
    sqlx::query("INSERT INTO workflow (id, name) VALUES ($1, 'old')")
        .bind(WorkflowId::generate().to_string())
        .execute(&mut conn)
        .await
        .unwrap();
    let workflow_id: String = sqlx::query_scalar("SELECT id FROM workflow")
        .fetch_one(&mut conn)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO workflow_version (workflow_id, version, source_yaml, schema_hash)
         VALUES ($1, 1, 'name: old', 'h')",
    )
    .bind(&workflow_id)
    .execute(&mut conn)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO task (id, title, body, workflow_id, status) VALUES ($1, 't', '', $2, 'backlog')",
    )
    .bind(TaskId::generate().to_string())
    .bind(&workflow_id)
    .execute(&mut conn)
    .await
    .unwrap();
    conn.close().await.unwrap();

    let store = Store::open(dir.path()).await.unwrap();
    let mut tx = store.read().await.unwrap();
    let workflows = tx.workflows().await.unwrap();
    assert_eq!(workflows.len(), 1);
    assert_eq!(workflows[0].workspace_id, WorkspaceId::MAIN);
    assert_eq!(
        tx.tasks_of_workspace(WorkspaceId::MAIN)
            .await
            .unwrap()
            .len(),
        1
    );
    assert!(
        tx.latest_workflow_version(workflows[0].id)
            .await
            .unwrap()
            .is_some()
    );
    let mut conn = SqliteConnection::connect(&url).await.unwrap();
    let violations: Vec<(String,)> =
        sqlx::query_as("SELECT \"table\" FROM pragma_foreign_key_check")
            .fetch_all(&mut conn)
            .await
            .unwrap();
    assert!(violations.is_empty(), "{violations:?}");
    // workspace_id is NOT NULL (by trigger) on insert and update.
    let without = sqlx::query("INSERT INTO workflow (id, name) VALUES ($1, 'x')")
        .bind(WorkflowId::generate().to_string())
        .execute(&mut conn)
        .await;
    assert!(without.is_err());
    let cleared = sqlx::query("UPDATE workflow SET workspace_id = NULL")
        .execute(&mut conn)
        .await;
    assert!(cleared.is_err());
}

#[tokio::test]
async fn repos_belong_to_one_workspace() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).await.unwrap();
    let mut tx = store.begin().await.unwrap();
    let ops = workspace("Ops");
    tx.insert_workspace(&ops).await.unwrap();
    let repo = |workspace_id, url: &str| Repo {
        id: RepoId::generate(),
        workspace_id,
        url: url.into(),
        default_branch: "main".into(),
        gate_config: serde_json::json!({}),
        description: "the ops tooling".into(),
        hook_id: None,
    };
    let tooling = repo(ops.id, "https://example.com/ops/tooling.git");
    tx.insert_repo(&tooling).await.unwrap();
    // One URL per workspace; another workspace may add the same URL.
    assert!(invalid(tx.insert_repo(&repo(ops.id, &tooling.url)).await));
    let on_main = repo(WorkspaceId::MAIN, &tooling.url);
    tx.insert_repo(&on_main).await.unwrap();
    assert_eq!(tx.repos(ops.id).await.unwrap(), vec![tooling.clone()]);
    assert_eq!(tx.repos(WorkspaceId::MAIN).await.unwrap(), vec![on_main]);
    assert_eq!(
        tx.repo_by_url(ops.id, &tooling.url).await.unwrap(),
        Some(tooling.clone())
    );
    assert_eq!(tx.repo_by_url(ops.id, "https://x").await.unwrap(), None);
    // Edits, the hook, deletion.
    tx.update_repo(tooling.id, "ops scripts", "develop")
        .await
        .unwrap();
    tx.set_hook(tooling.id, Some(77)).await.unwrap();
    let saved = tx.get_repo(tooling.id).await.unwrap();
    assert_eq!(
        (
            saved.description.as_str(),
            saved.default_branch.as_str(),
            saved.hook_id
        ),
        ("ops scripts", "develop", Some(77))
    );
    assert!(tx.delete_repo(tooling.id).await.unwrap());
    assert!(!tx.delete_repo(tooling.id).await.unwrap());
    assert!(matches!(
        tx.get_repo(tooling.id).await,
        Err(StoreError::Core(Error::NotFound(_)))
    ));
    assert!(matches!(
        tx.set_hook(tooling.id, None).await,
        Err(StoreError::Core(Error::NotFound(_)))
    ));
    // A workspace's repos go with it.
    tx.insert_repo(&repo(ops.id, "https://example.com/ops/gone"))
        .await
        .unwrap();
    tx.delete_workspace(ops.id).await.unwrap();
    assert!(tx.repos(ops.id).await.unwrap().is_empty());
}

#[tokio::test]
async fn workspace_links_knowledge_sources_as_one_row() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).await.unwrap();
    let mut tx = store.begin().await.unwrap();
    let ops = workspace("Ops");
    tx.insert_workspace(&ops).await.unwrap();
    let source = KnowledgeSource {
        id: "wiki".into(),
        kind: "upload".into(),
        name: "Wiki".into(),
        config: serde_json::json!({}),
    };
    tx.insert_knowledge_source(&source, WorkspaceId::MAIN)
        .await
        .unwrap();

    // It lands on its own workspace only.
    assert!(tx.knowledge_sources(Some(ops.id)).await.unwrap().is_empty());
    assert_eq!(
        tx.source_ids_of_workspace(WorkspaceId::MAIN).await.unwrap(),
        vec!["wiki".to_owned()]
    );

    // Importing links the same row; twice is a no-op.
    let wiki = WorkspaceItem::KnowledgeSource("wiki");
    tx.link(ops.id, wiki).await.unwrap();
    tx.link(ops.id, wiki).await.unwrap();
    let mut both = vec![WorkspaceId::MAIN, ops.id];
    both.sort();
    assert_eq!(tx.workspaces_of(wiki).await.unwrap(), both);
    assert_eq!(tx.knowledge_sources(None).await.unwrap().len(), 1);
    assert!(tx.unlink(WorkspaceId::MAIN, wiki).await.unwrap());
    assert!(!tx.unlink(WorkspaceId::MAIN, wiki).await.unwrap());
    assert_eq!(tx.workspaces_of(wiki).await.unwrap(), vec![ops.id]);
    // Deleting the source drops its links.
    tx.delete_knowledge_source("wiki").await.unwrap();
    assert!(tx.workspaces_of(wiki).await.unwrap().is_empty());
}

/// Sources from before workspaces land on Main; repos from then are dropped
/// by the git migration (repos are per workspace now, re-added by hand).
#[tokio::test]
async fn workspace_links_migration_puts_existing_sources_on_main_and_drops_old_repos() {
    let dir = tempfile::tempdir().unwrap();
    let url = format!("sqlite://{}?mode=rwc", dir.path().join("sun.db").display());
    let mut conn = SqliteConnection::connect(&url).await.unwrap();
    conn.ensure_migrations_table().await.unwrap();
    let migrator = sqlx::migrate!("./migrations");
    for migration in migrator.iter().filter(|m| m.version < 20261012090000) {
        conn.apply(migration).await.unwrap();
    }
    let repo_id = RepoId::generate();
    sqlx::query("INSERT INTO repo (id, url, default_branch) VALUES ($1, 'u', 'main')")
        .bind(repo_id.to_string())
        .execute(&mut conn)
        .await
        .unwrap();
    sqlx::query("INSERT INTO knowledge_source (id, kind, name) VALUES ('wiki', 'upload', 'Wiki')")
        .execute(&mut conn)
        .await
        .unwrap();
    conn.close().await.unwrap();

    let store = Store::open(dir.path()).await.unwrap();
    let mut tx = store.read().await.unwrap();
    assert!(tx.repos(WorkspaceId::MAIN).await.unwrap().is_empty());
    assert!(tx.get_repo(repo_id).await.is_err());
    assert_eq!(
        tx.source_ids_of_workspace(WorkspaceId::MAIN).await.unwrap(),
        vec!["wiki".to_owned()]
    );
}

#[tokio::test]
async fn profile_migration_gives_every_workspace_a_unique_slug_and_prefix() {
    let dir = tempfile::tempdir().unwrap();
    let url = format!("sqlite://{}?mode=rwc", dir.path().join("sun.db").display());
    let mut conn = SqliteConnection::connect(&url).await.unwrap();
    conn.ensure_migrations_table().await.unwrap();
    let migrator = sqlx::migrate!("./migrations");
    for migration in migrator.iter().filter(|m| m.version < 20261019090000) {
        conn.apply(migration).await.unwrap();
    }
    for (id, name) in [
        ("01K0000000000000000000000A", "My App"),
        ("01K0000000000000000000000B", "my-app!"),
        ("01K0000000000000000000000C", "$$$"),
    ] {
        sqlx::query("INSERT INTO workspace (id, name, created_at) VALUES ($1, $2, '2026-10-01T00:00:00+00:00')")
            .bind(id)
            .bind(name)
            .execute(&mut conn)
            .await
            .unwrap();
    }
    conn.close().await.unwrap();

    let store = Store::open(dir.path()).await.unwrap();
    let mut tx = store.read().await.unwrap();
    let got: Vec<(String, String)> = tx
        .workspaces()
        .await
        .unwrap()
        .into_iter()
        .map(|w| (w.slug, w.issue_prefix))
        .collect();
    // The task migration then makes prefixes unique: the later workspace
    // gets a digit.
    let want = [
        ("main", "MAIN"),
        ("my-app", "MYAP"),
        ("my-app-2", "MYAP2"),
        ("workspace", "WORK"),
    ];
    let want: Vec<(String, String)> = want
        .iter()
        .map(|(s, p)| ((*s).into(), (*p).into()))
        .collect();
    assert_eq!(got, want);
    // The slug never changes.
    let mut conn = SqliteConnection::connect(&url).await.unwrap();
    let changed = sqlx::query("UPDATE workspace SET slug = 'x'")
        .execute(&mut conn)
        .await;
    assert!(changed.is_err());
}

const PNG: &[u8] = b"\x89PNG\r\n\x1a\n a logo";

/// Rows of `workspace_logo` in the data dir `dir`, by workspace.
async fn logo_rows(dir: &std::path::Path) -> Vec<(String, String)> {
    let url = format!("sqlite://{}", dir.join("sun.db").display());
    let mut conn = SqliteConnection::connect(&url).await.unwrap();
    let rows =
        sqlx::query_as("SELECT workspace_id, hash FROM workspace_logo ORDER BY workspace_id")
            .fetch_all(&mut conn)
            .await
            .unwrap();
    conn.close().await.unwrap();
    rows
}

#[tokio::test]
async fn a_logo_is_its_workspaces_own_row() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).await.unwrap();
    let ops = workspace("Ops");
    let mut tx = store.begin().await.unwrap();
    tx.insert_workspace(&ops).await.unwrap();
    for id in [WorkspaceId::MAIN, ops.id] {
        tx.set_logo(id, Some(("image/png", PNG))).await.unwrap();
    }
    let hash = tx.get_workspace(ops.id).await.unwrap().logo_hash.unwrap();
    assert_eq!(hash.len(), 64);
    assert_eq!(
        tx.logo(ops.id).await.unwrap(),
        Some(("image/png".to_owned(), PNG.to_vec()))
    );
    // Replaced: one row with the new image; the same image elsewhere stays.
    let other = [PNG, b"other"].concat();
    tx.set_logo(ops.id, Some(("image/png", &other)))
        .await
        .unwrap();
    assert_eq!(tx.logo(ops.id).await.unwrap().unwrap().1, other);
    assert_ne!(
        tx.get_workspace(ops.id).await.unwrap().logo_hash.unwrap(),
        hash
    );
    assert_eq!(tx.logo(WorkspaceId::MAIN).await.unwrap().unwrap().1, PNG);
    // Removed: no row; deleting the workspace takes its logo too.
    tx.set_logo(WorkspaceId::MAIN, None).await.unwrap();
    assert_eq!(tx.logo(WorkspaceId::MAIN).await.unwrap(), None);
    assert_eq!(
        tx.get_workspace(WorkspaceId::MAIN).await.unwrap().logo_hash,
        None
    );
    tx.delete_workspace(ops.id).await.unwrap();
    tx.commit().await.unwrap();
    assert!(logo_rows(dir.path()).await.is_empty(), "no row left behind");
    let mut tx = store.begin().await.unwrap();
    assert!(tx.set_logo(ops.id, Some(("image/png", PNG))).await.is_err());
}

#[tokio::test]
async fn task_migration_moves_old_logos_out_of_the_blob_store() {
    // Straight across both migrations, and from an install that already
    // has `workspace_logo` (the move used to run at open).
    for before in [20261022090000, 20261022100000] {
        old_logos_move_from(before).await;
    }
}

async fn old_logos_move_from(before: i64) {
    let dir = tempfile::tempdir().unwrap();
    // The image as a blob of another data dir, copied over like a real one.
    let other = tempfile::tempdir().unwrap();
    let hash = Store::open(other.path())
        .await
        .unwrap()
        .blobs()
        .put(PNG.to_vec())
        .await
        .unwrap();
    let file = |root: &std::path::Path| {
        root.join("blobs")
            .join(&hash.as_str()[..2])
            .join(format!("{}.zst", hash.as_str()))
    };
    std::fs::create_dir_all(file(dir.path()).parent().unwrap()).unwrap();
    std::fs::copy(file(other.path()), file(dir.path())).unwrap();

    let url = format!("sqlite://{}?mode=rwc", dir.path().join("sun.db").display());
    let mut conn = SqliteConnection::connect(&url).await.unwrap();
    conn.ensure_migrations_table().await.unwrap();
    let migrator = sqlx::migrate!("./migrations");
    for migration in migrator.iter().filter(|m| m.version < before) {
        conn.apply(migration).await.unwrap();
    }
    sqlx::query("UPDATE workspace SET logo_blob = $1")
        .bind(hash.as_str())
        .execute(&mut conn)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO workspace (id, name, slug, issue_prefix, created_at, logo_blob)
         VALUES ('01K0000000000000000000000B', 'Gone', 'gone', 'GONE',
                 '2026-10-01T00:00:00+00:00', $1)",
    )
    .bind("b".repeat(64))
    .execute(&mut conn)
    .await
    .unwrap();
    conn.close().await.unwrap();

    let store = Store::open(dir.path()).await.unwrap();
    let mut tx = store.read().await.unwrap();
    assert_eq!(
        tx.logo(WorkspaceId::MAIN).await.unwrap(),
        Some(("image/png".to_owned(), PNG.to_vec()))
    );
    let gone: WorkspaceId = "01K0000000000000000000000B".parse().unwrap();
    assert_eq!(tx.logo(gone).await.unwrap(), None, "its file was missing");
    drop(tx);
    drop(store);
    // The old column is gone, and a second open has nothing left to do.
    let columns = || async {
        let mut conn = SqliteConnection::connect(&url).await.unwrap();
        let names: Vec<String> =
            sqlx::query_scalar("SELECT name FROM pragma_table_info('workspace')")
                .fetch_all(&mut conn)
                .await
                .unwrap();
        let applied: i64 = sqlx::query_scalar("SELECT count(*) FROM _sqlx_migrations")
            .fetch_one(&mut conn)
            .await
            .unwrap();
        conn.close().await.unwrap();
        (names, applied)
    };
    let (names, applied) = columns().await;
    assert!(!names.iter().any(|n| n == "logo_blob"), "{names:?}");
    let store = Store::open(dir.path()).await.unwrap();
    let mut tx = store.read().await.unwrap();
    assert_eq!(tx.logo(WorkspaceId::MAIN).await.unwrap().unwrap().1, PNG);
    drop(tx);
    drop(store);
    assert_eq!(columns().await.1, applied, "no migration ran twice");
}
