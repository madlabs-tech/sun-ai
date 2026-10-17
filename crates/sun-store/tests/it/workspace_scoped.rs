//! Agents, runtimes, secrets and memory belong to a workspace
//! (`20261102090000`..`20261102090400`): checked on a copy of an
//! old-schema database, then the repositories keep each workspace's own.
#![allow(clippy::unwrap_used)]

use std::io::Write as _;
use std::path::Path;

use serde_json::json;
use sqlx::migrate::Migrate;
use sqlx::{Connection, SqliteConnection};
use sun_core::model::{
    Agent, GitSettings, MemoryItem, MemoryScope, MemoryStatus, Runtime, RuntimeSource, SecretScope,
    Workspace,
};
use sun_core::{AgentId, MemoryItemId, RuntimeId, WorkspaceId};
use sun_store::{
    AgentRepo, MasterKey, MemoryRepo, RuntimeRepo, SecretRepo, SecretToReenter, SettingRepo, Store,
    TaskRepo, WorkspaceRepo,
};

/// The first migration that puts things on workspaces.
const M4: i64 = 20261102090000;

const OPS: &str = "01K0000000000000000000000W";
const NIL: &str = "00000000000000000000000000";
const MAIN: &str = "00000000000000000000000001";
const ADA: &str = "01K00000000000000000000AD1";
const BOB: &str = "01K00000000000000000000B0B";
const W_OPS: &str = "01K00000000000000000000WF1";
const SYS_MAIN: &str = "01K00000000000000000000SY1";
const SYS_OPS: &str = "01K00000000000000000000SY2";
const T_MAIN: &str = "01K00000000000000000000TA1";
const T_OPS: &str = "01K00000000000000000000TA2";
const R_OPS: &str = "01K00000000000000000000RN1";
const CRED: &str = "01K00000000000000000000RT1";
const AT: &str = "2026-10-30T00:00:00+00:00";

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

async fn seal(conn: &mut SqliteConnection, key: &MasterKey, kind: &str, id: &str, name: &str) {
    let sealed = key
        .seal(
            format!("{kind}\0{id}\0{name}").as_bytes(),
            format!("{name}-value").as_bytes(),
        )
        .unwrap();
    sqlx::query(
        "INSERT INTO secret (id, scope_kind, scope_id, name, sealed, updated_at)
         VALUES ($1, $2, $3, $4, $5, $6)",
    )
    .bind(sun_core::SecretId::generate().to_string())
    .bind(kind)
    .bind(id)
    .bind(name)
    .bind(sealed)
    .bind(AT)
    .execute(&mut *conn)
    .await
    .unwrap();
}

/// A data dir migrated to just before [`M4`]: an agent Main and Ops both
/// use, a credits runtime, ecosystem secrets and memory from both.
async fn seed_old_schema(data: &Path, key: &MasterKey) {
    let mut file = std::fs::OpenOptions::new();
    file.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut file, 0o600);
    file.open(data.join("master.key"))
        .unwrap()
        .write_all(key.to_hex().as_bytes())
        .unwrap();
    let url = format!("sqlite://{}?mode=rwc", data.join("sun.db").display());
    let mut conn = SqliteConnection::connect(&url).await.unwrap();
    conn.ensure_migrations_table().await.unwrap();
    for migration in sqlx::migrate!("./migrations")
        .iter()
        .filter(|m| m.version < M4)
    {
        conn.apply(migration).await.unwrap();
    }
    let c = &mut conn;
    exec(
        c,
        "INSERT INTO workspace (id, name, slug, issue_prefix, created_at)
         VALUES ($1, 'Ops', 'ops', 'OPS', $2)",
        &[OPS, AT],
    )
    .await;
    for (id, name) in [(ADA, "Ada"), (BOB, "Bob")] {
        exec(
            c,
            "INSERT INTO agent (id, name, runtime_chain, created_at)
             VALUES ($1, $2, '[\"claude-code\"]', $3)",
            &[id, name, AT],
        )
        .await;
    }
    for (id, workspace, agent) in [
        (W_OPS, OPS, None),
        (SYS_MAIN, MAIN, Some(ADA)),
        (SYS_OPS, OPS, Some(ADA)),
    ] {
        sqlx::query(
            "INSERT INTO workflow (id, name, workspace_id, system_agent_id) VALUES ($1, 'w', $2, $3)",
        )
        .bind(id)
        .bind(workspace)
        .bind(agent)
        .execute(&mut *c)
        .await
        .unwrap();
        exec(
            c,
            "INSERT INTO workflow_version (workflow_id, version, source_yaml, schema_hash)
             VALUES ($1, 1, 'name: w', 'h')",
            &[id],
        )
        .await;
    }
    for (id, workspace, workflow) in [(T_MAIN, MAIN, SYS_MAIN), (T_OPS, OPS, SYS_OPS)] {
        exec(
            c,
            "INSERT INTO task (id, workspace_id, number, title, body, workflow_id, status,
                               assignee_agent_id, created_at, updated_at)
             VALUES ($1, $2, 1, 't', '', $3, 'todo', $4, $5, $5)",
            &[id, workspace, workflow, ADA, AT],
        )
        .await;
    }
    exec(
        c,
        "INSERT INTO run (id, task_id, workflow_id, workflow_version, status)
         VALUES ($1, $2, $3, 1, 'succeeded')",
        &[R_OPS, T_OPS, W_OPS],
    )
    .await;
    // From an Ops run; Ops' workflow, no run; the whole ecosystem.
    for (n, scope, scope_id, run) in [
        ("1", "repo", Some("01K00000000000000000000RE1"), Some(R_OPS)),
        ("2", "workflow", Some(W_OPS), None),
        ("3", "ecosystem", None, None),
    ] {
        sqlx::query(
            "INSERT INTO memory_item (id, scope, scope_id, body, reason, source_run, status,
                                      author, created_at, dedupe_key)
             VALUES ($1, $2, $3, 'b', 'r', $4, 'active', 'sun', $5, $1)",
        )
        .bind(format!("01K00000000000000000000ME{n}"))
        .bind(scope)
        .bind(scope_id)
        .bind(run)
        .bind(AT)
        .execute(&mut *c)
        .await
        .unwrap();
    }
    exec(
        c,
        "INSERT INTO runtime (id, name, adapter, source) VALUES ($1, 'cred', 'opencode', 'credits')",
        &[CRED],
    )
    .await;
    exec(
        c,
        "INSERT INTO kernel_setting (key, value_json) VALUES ('credits_model:cred', '\"m\"')",
        &[],
    )
    .await;
    for name in [
        "OR_KEY",
        "SMTP_PASSWORD",
        "SSO_GOOGLE_SECRET",
        "GITHUB_APP_ID",
        "CLASH",
    ] {
        seal(c, key, "ecosystem", NIL, name).await;
    }
    seal(c, key, "workspace", MAIN, "CLASH").await;
    let reenter = json!([
        {"scope": {"kind": "ecosystem", "id": NIL}, "name": "JIRA_TOKEN"},
        {"scope": {"kind": "ecosystem", "id": NIL}, "name": "EMBEDDING_API_KEY"},
    ]);
    exec(
        c,
        "INSERT INTO kernel_setting (key, value_json) VALUES ('secrets_to_reenter', $1)",
        &[&reenter.to_string()],
    )
    .await;
    conn.close().await.unwrap();
}

#[tokio::test]
async fn agents_runtimes_secrets_and_memory_move_into_workspaces_on_an_old_database_copy() {
    let dir = tempfile::tempdir().unwrap();
    let old = dir.path().join("old");
    std::fs::create_dir_all(&old).unwrap();
    let key = MasterKey::generate().unwrap();
    seed_old_schema(&old, &key).await;
    let data = dir.path().join("copy");
    std::fs::create_dir_all(&data).unwrap();
    for file in ["sun.db", "master.key"] {
        std::fs::copy(old.join(file), data.join(file)).unwrap();
    }

    let store = Store::open(&data).await.unwrap();
    let mut tx = store.read().await.unwrap();
    let (main, ops) = (WorkspaceId::MAIN, OPS.parse::<WorkspaceId>().unwrap());

    // Agents: in Main; Ops' use of Ada moved to a copy of it in Ops.
    let ada: AgentId = ADA.parse().unwrap();
    let names = |agents: Vec<Agent>| {
        agents
            .into_iter()
            .map(|a| (a.name, a.id))
            .collect::<Vec<_>>()
    };
    assert_eq!(
        names(tx.agents(Some(main)).await.unwrap()),
        [
            ("Ada".to_owned(), ada),
            ("Bob".to_owned(), BOB.parse().unwrap())
        ]
    );
    let [copy] = tx.agents(Some(ops)).await.unwrap().try_into().unwrap();
    assert_eq!((copy.name.as_str(), copy.workspace_id), ("Ada", ops));
    assert_ne!(copy.id, ada);
    assert_eq!(copy.runtime_chain, ["claude-code"]);
    let task = |id: &str| id.parse().unwrap();
    assert_eq!(
        tx.get_task(task(T_OPS)).await.unwrap().assignee_agent_id,
        Some(copy.id)
    );
    assert_eq!(
        tx.get_task(task(T_MAIN)).await.unwrap().assignee_agent_id,
        Some(ada)
    );
    assert_eq!(
        tx.system_workflow(copy.id, ops).await.unwrap(),
        Some(SYS_OPS.parse().unwrap())
    );
    assert_eq!(
        tx.system_agent_of(SYS_MAIN.parse().unwrap()).await.unwrap(),
        Some(ada)
    );
    assert_eq!(tx.system_workflow(ada, ops).await.unwrap(), None);

    // Runtimes: in Main, ids kept; the gateway model is keyed by id.
    let cred = tx.runtime_by_name(main, "cred").await.unwrap().unwrap();
    assert_eq!((cred.id, cred.created_by), (CRED.parse().unwrap(), None));
    assert!(tx.runtime_by_name(ops, "cred").await.unwrap().is_none());
    assert_eq!(
        tx.setting(&format!("credits_model:{CRED}")).await.unwrap(),
        Some(json!("m"))
    );
    assert_eq!(tx.setting("credits_model:cred").await.unwrap(), None);

    // Secrets: a person's go to Main, re-sealed; the deployment's and a
    // name Main already had stay.
    let eco = SecretScope::Ecosystem(NIL.parse().unwrap());
    let in_main = SecretScope::Workspace(main);
    let listed = |secrets: Vec<sun_core::model::Secret>| {
        secrets.into_iter().map(|s| s.name).collect::<Vec<_>>()
    };
    assert_eq!(
        listed(tx.list_secrets(Some(&eco)).await.unwrap()),
        [
            "CLASH",
            "GITHUB_APP_ID",
            "SMTP_PASSWORD",
            "SSO_GOOGLE_SECRET"
        ]
    );
    assert_eq!(
        listed(tx.list_secrets(Some(&in_main)).await.unwrap()),
        ["CLASH", "OR_KEY"]
    );
    for (scope, name) in [
        (&in_main, "OR_KEY"),
        (&in_main, "CLASH"),
        (&eco, "CLASH"),
        (&eco, "SMTP_PASSWORD"),
    ] {
        let value = tx.secret_value(scope, name).await.unwrap();
        assert_eq!(value, format!("{name}-value"));
    }
    assert_eq!(
        tx.secrets_to_reenter().await.unwrap(),
        [
            SecretToReenter {
                scope: in_main.clone(),
                name: "JIRA_TOKEN".into()
            },
            SecretToReenter {
                scope: eco.clone(),
                name: "EMBEDDING_API_KEY".into()
            },
        ]
    );

    // Memory: the source run's workspace, else the workflow's, else Main.
    let ids = |items: Vec<(MemoryItem, Option<sun_store::MemoryPr>)>| {
        items
            .into_iter()
            .map(|(m, _)| m.id.to_string())
            .collect::<Vec<_>>()
    };
    let mut in_ops = ids(tx.memory_items(Some(ops)).await.unwrap());
    in_ops.sort();
    assert_eq!(
        in_ops,
        ["01K00000000000000000000ME1", "01K00000000000000000000ME2"]
    );
    assert_eq!(
        ids(tx.memory_items(Some(main)).await.unwrap()),
        ["01K00000000000000000000ME3"]
    );
    assert_eq!(tx.active_memory(ops).await.unwrap().len(), 2);
    drop(tx);
    drop(store);
    // The original stays as it was.
    let url = format!("sqlite://{}", old.join("sun.db").display());
    let mut conn = SqliteConnection::connect(&url).await.unwrap();
    let agents: i64 = sqlx::query_scalar("SELECT count(*) FROM agent")
        .fetch_one(&mut conn)
        .await
        .unwrap();
    assert_eq!(agents, 2);
    conn.close().await.unwrap();
}

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
        created_by: None,
        created_at: chrono::Utc::now(),
    }
}

fn agent(workspace: WorkspaceId, name: &str) -> Agent {
    Agent {
        id: AgentId::generate(),
        workspace_id: workspace,
        name: name.into(),
        description: String::new(),
        instructions: String::new(),
        runtime_chain: vec!["claude-code".into()],
        skill_ids: Vec::new(),
        permission_mode: None,
        created_at: chrono::Utc::now(),
        archived_at: None,
    }
}

fn memory(workspace: WorkspaceId) -> MemoryItem {
    MemoryItem {
        id: MemoryItemId::generate(),
        workspace_id: workspace,
        scope: MemoryScope::Ecosystem,
        scope_id: None,
        body: "Run the tests.".into(),
        target: None,
        reason: "r".into(),
        source_run: None,
        status: MemoryStatus::Active,
        author: "sun".into(),
        reviewed_by: None,
        created_at: chrono::Utc::now(),
    }
}

#[tokio::test]
async fn each_workspace_keeps_its_own_agents_runtimes_and_memory() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).await.unwrap();
    let mut tx = store.begin().await.unwrap();
    let ops = workspace("Ops");
    tx.insert_workspace(&ops).await.unwrap();
    let (main, ops) = (WorkspaceId::MAIN, ops.id);

    // One name per workspace, ignoring case.
    let ada = agent(main, "Ada");
    tx.insert_agent(&ada).await.unwrap();
    let ops_ada = agent(ops, "ADA");
    tx.insert_agent(&ops_ada).await.unwrap();
    assert!(tx.insert_agent(&agent(ops, "ada")).await.is_err());
    assert_eq!(tx.agents(Some(ops)).await.unwrap(), [ops_ada]);
    assert_eq!(tx.agents(None).await.unwrap().len(), 2);

    // The same chain name is another runtime in each workspace.
    let byo = tx
        .upsert_runtime(main, "claude", "claude-code", RuntimeSource::ByoRuntime)
        .await
        .unwrap();
    let other = tx
        .upsert_runtime(ops, "claude", "codex", RuntimeSource::ByoRuntime)
        .await
        .unwrap();
    assert_ne!(byo.id, other.id);
    assert_eq!(
        tx.runtime_by_name(ops, "claude")
            .await
            .unwrap()
            .unwrap()
            .adapter,
        "codex"
    );
    let added = Runtime {
        id: RuntimeId::generate(),
        workspace_id: ops,
        name: "claude".into(),
        adapter: "opencode".into(),
        source: RuntimeSource::Credits,
        byo_key: None,
        created_by: None,
    };
    assert!(!tx.insert_runtime(&added).await.unwrap(), "taken in Ops");
    assert!(
        tx.insert_runtime(&Runtime {
            name: "credits".into(),
            ..added.clone()
        })
        .await
        .unwrap()
    );
    let in_ops: Vec<_> = tx
        .runtimes(Some(ops))
        .await
        .unwrap()
        .into_iter()
        .map(|r| r.name)
        .collect();
    assert_eq!(in_ops, ["claude", "credits"]);
    let cooled = format!("cred:{}", other.id);
    tx.set_cooldown(&cooled, Some(chrono::Utc::now()))
        .await
        .unwrap();

    // A run gets only its workspace's memory.
    let mine = memory(ops);
    tx.insert_memory(&mine, "a").await.unwrap();
    tx.insert_memory(&memory(main), "b").await.unwrap();
    assert_eq!(tx.active_memory(ops).await.unwrap(), [mine]);

    // Deleting the workspace takes them along; Main keeps its own.
    tx.delete_workspace(ops).await.unwrap();
    assert!(tx.agents(Some(ops)).await.unwrap().is_empty());
    assert!(tx.runtimes(Some(ops)).await.unwrap().is_empty());
    assert!(
        tx.cooldowns().await.unwrap().is_empty(),
        "its keys' cooldowns"
    );
    assert!(tx.active_memory(ops).await.unwrap().is_empty());
    assert_eq!(tx.agents(None).await.unwrap(), [ada]);
    assert_eq!(tx.runtimes(None).await.unwrap(), [byo]);
    assert_eq!(tx.active_memory(main).await.unwrap().len(), 1);
    tx.commit().await.unwrap();
}
