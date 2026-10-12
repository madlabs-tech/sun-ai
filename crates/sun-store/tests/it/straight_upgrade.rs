//! A data dir from before the workspace rename (boards, `*_board`
//! permission keys, the old install-wide `workspace` secret scope, int
//! priorities and the old task statuses) opens on today's store in one go:
//! rename, profile (slugs, prefixes), projects, git connections, the task
//! rebuild, the logo and secret data steps and agents, all in a single open.
//! A second open changes no row.
#![allow(clippy::unwrap_used)]

use std::path::Path;

use sqlx::migrate::Migrate;
use sqlx::{Connection, SqliteConnection};
use sun_core::model::{AuditAction, Permission, Priority, SecretScope, TaskKey};
use sun_core::{TaskStatus, WorkspaceId};
use sun_store::{
    AuditRepo, GroupRepo, RepoRepo, SecretRepo, Store, StoreError, TaskRepo, WorkflowRepo,
    WorkspaceRepo,
};

/// The first migration of the rename (T21).
const T21: i64 = 20261018090000;
const OPS: &str = "01K0000000000000000000000B";
const OPS2: &str = "01K0000000000000000000000C";
const MAINFRAME: &str = "01K0000000000000000000000D";
const SYMBOLS: &str = "01K0000000000000000000000E";
const LEADS: &str = "01K0000000000000000000000G";
const MAIN_FLOW: &str = "01K000000000000000000000F1";
const OPS_FLOW: &str = "01K000000000000000000000F2";
const REPO: &str = "01K000000000000000000000R1";
const NIL: &str = "00000000000000000000000000";

/// Old tasks: id, workflow, status, priority → new status, number.
const TASKS: [(&str, &str, &str, i64, TaskStatus, u32); 4] = [
    (
        "01K00000010000000000000000",
        MAIN_FLOW,
        "queued",
        5,
        TaskStatus::Todo,
        1,
    ),
    (
        "01K00000020000000000000000",
        OPS_FLOW,
        "done",
        -1,
        TaskStatus::Done,
        1,
    ),
    (
        "01K00000030000000000000000",
        OPS_FLOW,
        "needs_attention",
        0,
        TaskStatus::NeedsYou,
        2,
    ),
    (
        "01K00000040000000000000000",
        MAIN_FLOW,
        "backlog",
        0,
        TaskStatus::Backlog,
        2,
    ),
];

async fn exec(conn: &mut SqliteConnection, sql: &str, binds: &[&str]) {
    let mut query = sqlx::query(sql);
    for bind in binds {
        query = query.bind(*bind);
    }
    query
        .execute(&mut *conn)
        .await
        .unwrap_or_else(|e| panic!("{sql}: {e}"));
}

/// A data dir migrated to just before T21, with boards and the rest.
async fn seed_pre_rename(data: &Path) {
    let url = format!("sqlite://{}?mode=rwc", data.join("sun.db").display());
    let mut conn = SqliteConnection::connect(&url).await.unwrap();
    conn.ensure_migrations_table().await.unwrap();
    for migration in sqlx::migrate!("./migrations")
        .iter()
        .filter(|m| m.version < T21)
    {
        conn.apply(migration).await.unwrap();
    }
    let at = "2026-10-01T00:00:00+00:00";
    // Names whose slugs and prefixes collide once derived.
    for (id, name) in [
        (OPS, "Ops"),
        (OPS2, "Ops!"),
        (MAINFRAME, "Mainframe"),
        (SYMBOLS, "$$$"),
    ] {
        exec(
            &mut conn,
            "INSERT INTO board (id, name, created_at) VALUES ($1, $2, $3)",
            &[id, name, at],
        )
        .await;
    }
    exec(
        &mut conn,
        "INSERT INTO user_group (id, key, name, permissions_json) VALUES ($1, 'leads', 'Leads',
         '[\"run\",\"create_board\",\"edit_board\",\"delete_board\",\"access_all_boards\"]')",
        &[LEADS],
    )
    .await;
    exec(
        &mut conn,
        "INSERT INTO board_group (board_id, group_id) VALUES ($1, $2)",
        &[OPS, LEADS],
    )
    .await;
    for (flow, board) in [(MAIN_FLOW, "00000000000000000000000001"), (OPS_FLOW, OPS)] {
        exec(
            &mut conn,
            "INSERT INTO workflow (id, name, board_id) VALUES ($1, 'f', $2)",
            &[flow, board],
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
    for (id, flow, status, priority, ..) in TASKS {
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
    exec(
        &mut conn,
        "INSERT INTO repo (id, url, default_branch) VALUES ($1, 'https://example.com/ops', 'main')",
        &[REPO],
    )
    .await;
    exec(
        &mut conn,
        "INSERT INTO board_repo (board_id, repo_id) VALUES ($1, $2)",
        &[OPS, REPO],
    )
    .await;
    exec(
        &mut conn,
        "INSERT INTO memory_item (id, scope, body, reason, status, author, created_at)
         VALUES ('01K000000000000000000000M1', 'workspace', 'b', 'r', 'proposed', 'me', $1)",
        &[at],
    )
    .await;
    exec(
        &mut conn,
        "INSERT INTO audit (id, actor, action, target, detail, at)
         VALUES ('01K000000000000000000000A1', 'user:me', 'board_change', 'Ops', 'created', $1)",
        &[at],
    )
    .await;
    // The install-wide secret as it was sealed then: scope `workspace`.
    let key = sun_store::MasterKey::load(data).unwrap();
    let sealed = key
        .seal(
            format!("workspace\0{NIL}\0API_TOKEN").as_bytes(),
            b"dummy-old-install-value",
        )
        .unwrap();
    sqlx::query(
        "INSERT INTO secret (id, scope_kind, scope_id, name, sealed, updated_at)
         VALUES ('01K000000000000000000000S1', 'workspace', $1, 'API_TOKEN', $2, $3)",
    )
    .bind(NIL)
    .bind(sealed)
    .bind(at)
    .execute(&mut conn)
    .await
    .unwrap();
    conn.close().await.unwrap();
}

/// Every row of every table (virtual tables aside), as text, by table.
async fn dump(data: &Path) -> Vec<(String, Vec<String>)> {
    let url = format!("sqlite://{}", data.join("sun.db").display());
    let mut conn = SqliteConnection::connect(&url).await.unwrap();
    let tables: Vec<String> = sqlx::query_scalar(
        "SELECT name FROM sqlite_schema WHERE type = 'table' AND name NOT LIKE 'sqlite_%'
         AND sql NOT LIKE 'CREATE VIRTUAL%' ORDER BY name",
    )
    .fetch_all(&mut conn)
    .await
    .unwrap();
    let mut out = Vec::new();
    for table in tables {
        let columns: Vec<String> =
            sqlx::query_scalar("SELECT name FROM pragma_table_info($1) ORDER BY cid")
                .bind(&table)
                .fetch_all(&mut conn)
                .await
                .unwrap();
        let row = columns
            .iter()
            .map(|c| format!("quote(\"{c}\")"))
            .collect::<Vec<_>>()
            .join(" || '|' || ");
        let rows: Vec<String> =
            match sqlx::query_scalar(&format!("SELECT {row} FROM \"{table}\" ORDER BY 1"))
                .fetch_all(&mut conn)
                .await
            {
                Ok(rows) => rows,
                // A shadow table of a virtual one that can't be read directly.
                Err(_) => continue,
            };
        out.push((table, rows));
    }
    conn.close().await.unwrap();
    out
}

/// The schema SQL of a data dir, sorted.
async fn schema(data: &Path) -> Vec<String> {
    let url = format!("sqlite://{}", data.join("sun.db").display());
    let mut conn = SqliteConnection::connect(&url).await.unwrap();
    let sql = sqlx::query_scalar(
        "SELECT coalesce(sql, '') FROM sqlite_schema WHERE name NOT LIKE 'sqlite_%' ORDER BY name",
    )
    .fetch_all(&mut conn)
    .await
    .unwrap();
    conn.close().await.unwrap();
    sql
}

#[tokio::test]
async fn a_pre_rename_data_dir_upgrades_through_every_migration_in_one_open() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path();
    seed_pre_rename(data).await;

    let store = Store::open(data).await.unwrap();
    assert!(data.join("backups").exists(), "backed up before migrating");
    let mut tx = store.read().await.unwrap();

    // Boards are workspaces: slugs from their names, prefixes unique.
    let got: Vec<(String, String, String)> = tx
        .workspaces()
        .await
        .unwrap()
        .into_iter()
        .map(|w| (w.name, w.slug, w.issue_prefix))
        .collect();
    let want = [
        ("Main", "main", "MAIN"),
        ("Ops", "ops", "OPS"),
        ("Ops!", "ops-2", "OPS2"),
        ("Mainframe", "mainframe", "MAIN2"),
        ("$$$", "workspace", "WORK"),
    ];
    let want: Vec<(String, String, String)> = want
        .iter()
        .map(|(n, s, p)| ((*n).into(), (*s).into(), (*p).into()))
        .collect();
    assert_eq!(got, want);

    // Permission keys and access links carried over.
    let groups = tx.groups().await.unwrap();
    let leads = groups.iter().find(|g| g.key == "leads").unwrap();
    for permission in [
        Permission::Run,
        Permission::CreateWorkspace,
        Permission::EditWorkspace,
        Permission::DeleteWorkspace,
        Permission::AccessAllWorkspaces,
    ] {
        assert!(leads.permissions.contains(&permission), "{permission}");
    }
    let ops: WorkspaceId = OPS.parse().unwrap();
    let links = tx.workspace_groups().await.unwrap();
    assert!(links.contains(&(ops, LEADS.parse().unwrap())), "{links:?}");
    assert_eq!(
        tx.get_workflow(OPS_FLOW.parse().unwrap())
            .await
            .unwrap()
            .workspace_id,
        ops
    );
    // Old repo rows are dropped, not migrated (repos are per workspace now).
    assert!(matches!(
        tx.get_repo(REPO.parse().unwrap()).await,
        Err(StoreError::Core(sun_core::Error::NotFound(_)))
    ));
    assert!(tx.repos(ops).await.unwrap().is_empty());

    // Tasks rebuilt: numbered per workspace, statuses and priorities mapped,
    // keys resolve, counters continue.
    for (id, flow, _, priority, status, number) in TASKS {
        let task = tx.get_task(id.parse().unwrap()).await.unwrap();
        assert_eq!((task.status, task.number), (status, number), "{id}");
        assert_eq!(task.priority, Priority::from_int(priority as i32), "{id}");
        let workspace = if flow == OPS_FLOW {
            ops
        } else {
            WorkspaceId::MAIN
        };
        assert_eq!(task.workspace_id, workspace, "{id}");
        assert_eq!(task.assignee_agent_id, None);
    }
    let key: TaskKey = "ops-2".parse().unwrap();
    assert_eq!(
        tx.task_by_key(&key).await.unwrap().id,
        TASKS[2].0.parse().unwrap()
    );
    assert_eq!(tx.get_workspace(ops).await.unwrap().issue_counter, 2);

    // The old audit row reads as a workspace change.
    let audit = tx.audit_log(None, 100).await.unwrap();
    assert!(
        audit
            .iter()
            .any(|a| a.action == AuditAction::WorkspaceChange && a.target == "Ops"),
        "{audit:?}"
    );

    // The old install secret is an ecosystem secret, re-sealed so it opens.
    let ecosystem = SecretScope::Ecosystem(NIL.parse().unwrap());
    assert_eq!(
        tx.secret_value(&ecosystem, "API_TOKEN").await.unwrap(),
        "dummy-old-install-value"
    );
    drop(tx);
    drop(store);

    // Raw: memory scope, no leftover tables or columns, agents in place,
    // foreign keys hold, every migration applied once.
    let url = format!("sqlite://{}", data.join("sun.db").display());
    let mut conn = SqliteConnection::connect(&url).await.unwrap();
    let scope: String = sqlx::query_scalar("SELECT scope FROM memory_item")
        .fetch_one(&mut conn)
        .await
        .unwrap();
    assert_eq!(scope, "ecosystem");
    let leftovers: Vec<String> = sqlx::query_scalar(
        "SELECT name FROM sqlite_schema WHERE name LIKE '%board%' OR name = 'secret_to_reseal'
         OR name = 'task_new' OR name = 'task_activity_new' OR name = 'workspace_repo'",
    )
    .fetch_all(&mut conn)
    .await
    .unwrap();
    assert!(leftovers.is_empty(), "{leftovers:?}");
    let kinds: Vec<String> = sqlx::query_scalar("SELECT DISTINCT scope_kind FROM secret")
        .fetch_all(&mut conn)
        .await
        .unwrap();
    assert_eq!(kinds, ["ecosystem"]);
    let system: i64 =
        sqlx::query_scalar("SELECT count(*) FROM workflow WHERE system_agent_id IS NOT NULL")
            .fetch_one(&mut conn)
            .await
            .unwrap();
    assert_eq!(system, 0);
    let broken: Vec<String> = sqlx::query_scalar("SELECT \"table\" FROM pragma_foreign_key_check")
        .fetch_all(&mut conn)
        .await
        .unwrap();
    assert!(broken.is_empty(), "{broken:?}");
    let applied: Vec<i64> =
        sqlx::query_scalar("SELECT version FROM _sqlx_migrations WHERE success ORDER BY version")
            .fetch_all(&mut conn)
            .await
            .unwrap();
    let head: Vec<i64> = sqlx::migrate!("./migrations")
        .iter()
        .map(|m| m.version)
        .collect();
    assert_eq!(applied, head);
    conn.close().await.unwrap();

    // The upgraded schema is a fresh install's.
    let fresh = tempfile::tempdir().unwrap();
    drop(Store::open(fresh.path()).await.unwrap());
    assert_eq!(schema(data).await, schema(fresh.path()).await);

    // A second open changes nothing at all.
    let before = dump(data).await;
    let rows = |table: &str| {
        before
            .iter()
            .find(|(t, _)| t == table)
            .map(|(_, r)| r.len())
    };
    assert_eq!((rows("task"), rows("workspace")), (Some(4), Some(5)));
    let backups = std::fs::read_dir(data.join("backups")).unwrap().count();
    let store = Store::open(data).await.unwrap();
    drop(store);
    assert_eq!(dump(data).await, before);
    assert_eq!(
        std::fs::read_dir(data.join("backups")).unwrap().count(),
        backups,
        "nothing pending: no new backup"
    );
}

/// The git_multica migration (T1 of the multica port).
const GIT_MULTICA: i64 = 20261025090000;
const CONNECTION: &str = "01K000000000000000000000V1";

/// A data dir from just before the multica port: a repo added through a
/// GitHub connection and linked to Main with a GitLab-style hook id.
async fn seed_pre_multica(data: &Path) {
    let url = format!("sqlite://{}?mode=rwc", data.join("sun.db").display());
    let mut conn = SqliteConnection::connect(&url).await.unwrap();
    conn.ensure_migrations_table().await.unwrap();
    for migration in sqlx::migrate!("./migrations")
        .iter()
        .filter(|m| m.version < GIT_MULTICA)
    {
        conn.apply(migration).await.unwrap();
    }
    let at = "2026-10-20T00:00:00+00:00";
    let main = WorkspaceId::MAIN.to_string();
    exec(
        &mut conn,
        "INSERT INTO vcs_connection (id, workspace_id, provider, instance_url, account_login,
         installation_id, connected_by, created_at)
         VALUES ($1, $2, 'github', 'https://github.com', 'acme', 42, 'user:me', $3)",
        &[CONNECTION, &main, at],
    )
    .await;
    exec(
        &mut conn,
        "INSERT INTO repo (id, url, default_branch, connection_id, description)
         VALUES ($1, 'https://github.com/acme/web.git', 'main', $2, 'the site')",
        &[REPO, CONNECTION],
    )
    .await;
    exec(
        &mut conn,
        "INSERT INTO workspace_repo (workspace_id, repo_id, hook_id) VALUES ($1, $2, 77)",
        &[&main, REPO],
    )
    .await;
    conn.close().await.unwrap();
}

#[tokio::test]
async fn the_multica_migration_drops_old_repo_rows_and_keeps_the_connections() {
    use sun_core::model::VcsProvider;
    use sun_store::VcsRepo;

    let dir = tempfile::tempdir().unwrap();
    let data = dir.path();
    seed_pre_multica(data).await;

    let store = Store::open(data).await.unwrap();
    assert!(data.join("backups").exists(), "backed up before migrating");
    let mut tx = store.read().await.unwrap();
    // The old repo and its link are gone, not migrated (decision 3).
    assert!(matches!(
        tx.get_repo(REPO.parse().unwrap()).await,
        Err(StoreError::Core(sun_core::Error::NotFound(_)))
    ));
    assert!(tx.repos(WorkspaceId::MAIN).await.unwrap().is_empty());
    // The connection stays as it was.
    let connections = tx.connections(WorkspaceId::MAIN).await.unwrap();
    assert_eq!(connections.len(), 1, "{connections:?}");
    assert_eq!(connections[0].id, CONNECTION.parse().unwrap());
    assert_eq!(connections[0].provider, VcsProvider::Github);
    assert_eq!(connections[0].installation_id, Some(42));
    assert_eq!(connections[0].account_login, "acme");
    // The git rules read as all on.
    let workspace = tx.get_workspace(WorkspaceId::MAIN).await.unwrap();
    assert_eq!(workspace.git_settings, Default::default());
    drop(tx);
    drop(store);

    let url = format!("sqlite://{}", data.join("sun.db").display());
    let mut conn = SqliteConnection::connect(&url).await.unwrap();
    let tables: Vec<String> = sqlx::query_scalar(
        "SELECT name FROM sqlite_schema WHERE type = 'table' AND name IN
         ('workspace_repo', 'repo', 'pull_request', 'task_pull_request',
          'task_pull_request_exclusion', 'vcs_connection') ORDER BY name",
    )
    .fetch_all(&mut conn)
    .await
    .unwrap();
    assert_eq!(
        tables,
        [
            "pull_request",
            "repo",
            "task_pull_request",
            "task_pull_request_exclusion",
            "vcs_connection"
        ]
    );
    let columns = |table: &'static str| {
        sqlx::query_scalar::<_, String>("SELECT name FROM pragma_table_info($1)").bind(table)
    };
    let repo_columns: Vec<String> = columns("repo").fetch_all(&mut conn).await.unwrap();
    assert!(
        repo_columns.contains(&"workspace_id".to_owned()),
        "{repo_columns:?}"
    );
    assert!(
        !repo_columns.contains(&"connection_id".to_owned()),
        "{repo_columns:?}"
    );
    let task_columns: Vec<String> = columns("task").fetch_all(&mut conn).await.unwrap();
    assert!(
        !task_columns.contains(&"repo_ids_json".to_owned()),
        "{task_columns:?}"
    );
    assert!(
        task_columns.contains(&"pr_auto_complete_disabled".to_owned()),
        "{task_columns:?}"
    );
    conn.close().await.unwrap();
}
