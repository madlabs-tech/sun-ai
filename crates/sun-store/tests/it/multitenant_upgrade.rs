//! A realistic install from before multi-tenancy (groups and owners, a
//! second workspace, an agent both use, a BYO key runtime and its ecosystem
//! secret, the deployment's own secrets, memory of both workspaces, a host
//! and catalog installs) upgrades through every multi-tenant migration in
//! one open; then export → import keeps all of it readable.
#![allow(clippy::unwrap_used)]

use std::io::Write as _;
use std::path::Path;

use sqlx::migrate::Migrate;
use sqlx::{Connection, SqliteConnection};
use sun_core::model::{Role, SecretScope};
use sun_core::{AgentId, HostId, MemberId, WorkspaceId};
use sun_store::{
    AgentRepo, ArchiveSecrets, HostRepo, MasterKey, MembershipRepo, MemoryRepo, RuntimeRepo,
    SecretRepo, Store, TaskRepo, WorkspaceRepo,
};

/// The first multi-tenant migration (groups → workspace roles).
const M1: i64 = 20261101090000;

const MAIN: &str = "00000000000000000000000001";
const NIL: &str = "00000000000000000000000000";
const OPS: &str = "01K0000000000000000000000W";
const VIEWER_GROUP: &str = "00000000000000000000000001";
const ADMIN_GROUP: &str = "00000000000000000000000003";
const RUNNERS: &str = "01K0000000000000000000000R";
const OWNER: &str = "01K000000000000000000000M1";
const VIEWER: &str = "01K000000000000000000000M3";
const ADMIN: &str = "01K000000000000000000000M4";
const RUNNER: &str = "01K000000000000000000000M6";
const ADA: &str = "01K00000000000000000000AD1";
const SYS_MAIN: &str = "01K00000000000000000000SY1";
const SYS_OPS: &str = "01K00000000000000000000SY2";
const T_MAIN: &str = "01K00000000000000000000TA1";
const T_OPS: &str = "01K00000000000000000000TA2";
const R_OPS: &str = "01K00000000000000000000RN1";
const BYO: &str = "01K00000000000000000000RT2";
const HOST: &str = "01K00000000000000000000H01";
const MEM_OPS: &str = "01K00000000000000000000ME1";
const MEM_ALL: &str = "01K00000000000000000000ME3";
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

/// An ecosystem secret `name` sealed as the old store did, value `<name>-value`.
async fn seal(conn: &mut SqliteConnection, key: &MasterKey, name: &str) {
    let aad = format!("ecosystem\0{NIL}\0{name}");
    let sealed = key
        .seal(aad.as_bytes(), format!("{name}-value").as_bytes())
        .unwrap();
    sqlx::query(
        "INSERT INTO secret (id, scope_kind, scope_id, name, sealed, updated_at)
         VALUES ($1, 'ecosystem', $2, $3, $4, $5)",
    )
    .bind(sun_core::SecretId::generate().to_string())
    .bind(NIL)
    .bind(name)
    .bind(sealed)
    .bind(AT)
    .execute(&mut *conn)
    .await
    .unwrap();
}

/// A data dir migrated to just before [`M1`], filled as a team would have.
async fn seed(data: &Path, key: &MasterKey) {
    let mut file = std::fs::OpenOptions::new();
    file.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut file, 0o600);
    file.open(data.join("master.key"))
        .unwrap()
        .write_all(key.to_hex().as_bytes())
        .unwrap();
    std::fs::create_dir_all(data.join("catalog")).unwrap();
    std::fs::write(
        data.join("catalog/installed.json"),
        r#"{"tdd": "1.0.0", "playwright": "2.0.0"}"#,
    )
    .unwrap();
    let url = format!("sqlite://{}?mode=rwc", data.join("sun.db").display());
    let mut conn = SqliteConnection::connect(&url).await.unwrap();
    conn.ensure_migrations_table().await.unwrap();
    for migration in sqlx::migrate!("./migrations")
        .iter()
        .filter(|m| m.version < M1)
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
    // Groups: Viewer on Main and Ops, a custom group that runs, attached
    // nowhere (Main only, as before).
    exec(
        c,
        "INSERT INTO user_group (id, key, name, permissions_json)
         VALUES ($1, 'runners', 'Runners', '[\"run\"]')",
        &[RUNNERS],
    )
    .await;
    exec(
        c,
        "INSERT INTO workspace_group (workspace_id, group_id) VALUES ($1, $2)",
        &[OPS, VIEWER_GROUP],
    )
    .await;
    for (id, is_owner, group) in [
        (OWNER, "1", None),
        (VIEWER, "0", Some(VIEWER_GROUP)),
        (ADMIN, "0", Some(ADMIN_GROUP)),
        (RUNNER, "0", Some(RUNNERS)),
    ] {
        let email = format!("{id}@x.io");
        exec(
            c,
            "INSERT INTO member (id, issuer, subject, username, email, password_hash, is_owner)
             VALUES ($1, 'local', $2, $2, $2, 'h', $3)",
            &[id, &email, is_owner],
        )
        .await;
        if let Some(group) = group {
            exec(
                c,
                "INSERT INTO member_group (member_id, group_id) VALUES ($1, $2)",
                &[id, group],
            )
            .await;
        }
    }
    // An agent Main and Ops both use, through their system workflows.
    exec(
        c,
        "INSERT INTO agent (id, name, runtime_chain, created_at)
         VALUES ($1, 'Ada', '[\"or\"]', $2)",
        &[ADA, AT],
    )
    .await;
    for (id, workspace) in [(SYS_MAIN, MAIN), (SYS_OPS, OPS)] {
        exec(
            c,
            "INSERT INTO workflow (id, name, workspace_id, system_agent_id) VALUES ($1, 'w', $2, $3)",
            &[id, workspace, ADA],
        )
        .await;
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
        &[R_OPS, T_OPS, SYS_OPS],
    )
    .await;
    // A BYO key runtime whose key is an ecosystem secret, and the
    // deployment's own secrets.
    exec(
        c,
        "INSERT INTO runtime (id, name, adapter, source, config_json)
         VALUES ($1, 'or', 'opencode', 'byo_key', $2)",
        &[
            BYO,
            r#"{"kind":"openai_compatible","endpoint":"https://openrouter.ai/api/v1","model":"m","key_secret":"OR_KEY"}"#,
        ],
    )
    .await;
    for name in ["OR_KEY", "SMTP_PASSWORD", "SSO_GOOGLE_SECRET"] {
        seal(c, key, name).await;
    }
    // Memory from an Ops run, and one for everyone.
    for (id, scope, run) in [
        (MEM_OPS, "ecosystem", Some(R_OPS)),
        (MEM_ALL, "ecosystem", None),
    ] {
        sqlx::query(
            "INSERT INTO memory_item (id, scope, scope_id, body, reason, source_run, status,
                                      author, created_at, dedupe_key)
             VALUES ($1, $2, NULL, 'b', 'r', $3, 'active', 'sun', $4, $1)",
        )
        .bind(id)
        .bind(scope)
        .bind(run)
        .bind(AT)
        .execute(&mut *c)
        .await
        .unwrap();
    }
    exec(c, "INSERT INTO host (id, name) VALUES ($1, 'mac')", &[HOST]).await;
    conn.close().await.unwrap();
}

/// Everything the upgrade must have done, read through the repositories.
async fn check(store: &Store) {
    let (main, ops) = (WorkspaceId::MAIN, OPS.parse::<WorkspaceId>().unwrap());
    let mut tx = store.read().await.unwrap();
    let id = |s: &str| -> MemberId { s.parse().unwrap() };
    use Role::*;
    for (member, want) in [
        (OWNER, [Some(Owner), Some(Owner)]),
        (VIEWER, [Some(Viewer), Some(Viewer)]),
        (ADMIN, [Some(Admin), Some(Admin)]),
        (RUNNER, [Some(Member), None]),
    ] {
        let got = [
            tx.role_of(main, id(member)).await.unwrap(),
            tx.role_of(ops, id(member)).await.unwrap(),
        ];
        assert_eq!(got, want, "{member}");
    }

    // Ada stays Main's; Ops got its own copy, which its task now names.
    let ada: AgentId = ADA.parse().unwrap();
    let main_agents = tx.agents(Some(main)).await.unwrap();
    assert_eq!(main_agents.iter().map(|a| a.id).collect::<Vec<_>>(), [ada]);
    let [copy] = tx.agents(Some(ops)).await.unwrap().try_into().unwrap();
    assert_eq!(
        (copy.name.as_str(), copy.runtime_chain.as_slice()),
        ("Ada", &[sun_core::ChainEntry::from("or")][..])
    );
    let t_ops = tx.get_task(T_OPS.parse().unwrap()).await.unwrap();
    assert_eq!(t_ops.assignee_agent_id, Some(copy.id));

    // The BYO key runtime is Main's, its key a Main secret, still readable.
    let runtime = tx.runtime_by_name(main, "or").await.unwrap().unwrap();
    assert_eq!(runtime.id, BYO.parse().unwrap());
    let key = runtime.byo_key.unwrap().key_secret.unwrap();
    assert_eq!(key, "OR_KEY");
    let in_main = SecretScope::Workspace(main);
    assert_eq!(
        tx.secret_value(&in_main, &key).await.unwrap(),
        "OR_KEY-value"
    );
    let eco = SecretScope::Ecosystem(NIL.parse().unwrap());
    for name in ["SMTP_PASSWORD", "SSO_GOOGLE_SECRET"] {
        assert_eq!(
            tx.secret_value(&eco, name).await.unwrap(),
            format!("{name}-value")
        );
        assert!(
            tx.secret_value(&in_main, name).await.is_err(),
            "{name} stays the deployment's"
        );
    }
    assert!(
        tx.secret_value(&eco, "OR_KEY").await.is_err(),
        "moved, not copied"
    );
    assert!(tx.runtime_by_name(ops, "or").await.unwrap().is_none());

    // Memory: the Ops run's item is Ops', the other Main's.
    let ids = |items: Vec<(sun_core::model::MemoryItem, Option<sun_store::MemoryPr>)>| {
        items
            .into_iter()
            .map(|(m, _)| m.id.to_string())
            .collect::<Vec<_>>()
    };
    assert_eq!(ids(tx.memory_items(Some(ops)).await.unwrap()), [MEM_OPS]);
    assert_eq!(ids(tx.memory_items(Some(main)).await.unwrap()), [MEM_ALL]);

    // The host and the catalog installs are Main's.
    let host: HostId = HOST.parse().unwrap();
    let hosts = tx.hosts_in(main).await.unwrap();
    assert_eq!(hosts.iter().map(|h| h.id).collect::<Vec<_>>(), [host]);
    assert!(tx.hosts_in(ops).await.unwrap().is_empty());
    assert_eq!(
        tx.catalog_installs(main).await.unwrap(),
        ["playwright", "tdd"]
    );
    assert!(tx.catalog_installs(ops).await.unwrap().is_empty());
}

#[tokio::test]
async fn a_pre_multitenant_install_upgrades_in_one_open_and_round_trips() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("data");
    std::fs::create_dir_all(&data).unwrap();
    let key = MasterKey::generate().unwrap();
    seed(&data, &key).await;

    let store = Store::open(&data).await.unwrap();
    check(&store).await;
    // One backup, of the old schema, before the first migration.
    assert_eq!(std::fs::read_dir(data.join("backups")).unwrap().count(), 1);
    drop(store);
    // A second open changes nothing.
    let store = Store::open(&data).await.unwrap();
    check(&store).await;
    assert_eq!(std::fs::read_dir(data.join("backups")).unwrap().count(), 1);

    // Export → a fresh data dir → import: still all there and readable.
    let archive = dir.path().join("ecosystem.sunpack");
    store
        .export(&archive, ArchiveSecrets::WithKey)
        .await
        .unwrap();
    let fresh = dir.path().join("fresh");
    std::fs::create_dir_all(&fresh).unwrap();
    let imported = Store::import(&archive, &fresh).await.unwrap();
    check(&imported).await;
}
