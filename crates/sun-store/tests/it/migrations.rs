//! The migration files themselves: every one sorts after the tables it
//! needs, and a fresh store migrates to head.
#![allow(clippy::unwrap_used)]

use sqlx::Connection;
use sun_store::Store;

/// Every file in `migrations/` (version, SQL), sorted as sqlx applies them.
fn migrations() -> Vec<(i64, String)> {
    let dir = format!("{}/migrations", env!("CARGO_MANIFEST_DIR"));
    let mut files: Vec<(i64, String)> = std::fs::read_dir(&dir)
        .unwrap()
        .map(|entry| {
            let name = entry.unwrap().file_name().into_string().unwrap();
            let version = name.split('_').next().unwrap().parse().unwrap();
            (version, name)
        })
        .collect();
    files.sort();
    files
        .into_iter()
        .map(|(version, name)| {
            let sql = std::fs::read_to_string(format!("{dir}/{name}")).unwrap();
            (version, format!("-- file: {name}\n{sql}"))
        })
        .collect()
}

/// The tables a migration creates and the ones it needs to exist already.
/// `REFERENCES` is skipped: SQLite resolves it only when a row is written.
fn tables(sql: &str) -> (Vec<String>, Vec<String>) {
    let code: String = sql
        .lines()
        .map(|line| line.split("--").next().unwrap_or(""))
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase();
    let words: Vec<&str> = code
        .split(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
        .filter(|word| !word.is_empty())
        .collect();
    let (mut created, mut used) = (Vec::new(), Vec::new());
    for (i, pair) in words.windows(2).enumerate() {
        match pair {
            ["table", name] if i > 0 && matches!(words[i - 1], "create" | "virtual") => {
                let name = if *name == "if" { words[i + 4] } else { name };
                created.push(name.to_owned());
            }
            ["references", _] => {}
            ["table" | "on" | "into" | "update" | "from" | "join", name] => {
                used.push((*name).to_owned())
            }
            _ => {}
        }
    }
    (created, used)
}

/// A fresh database runs every migration in version order, so each one must
/// sort after the migrations creating the tables it alters, indexes or
/// triggers on.
#[test]
fn every_migration_sorts_after_the_tables_it_depends_on() {
    let files = migrations();
    let mut created_by: std::collections::HashMap<String, i64> = Default::default();
    for (version, sql) in &files {
        for table in tables(sql).0 {
            created_by.entry(table).or_insert(*version);
        }
    }
    for (version, sql) in &files {
        for table in tables(sql).1 {
            if let Some(creator) = created_by.get(&table) {
                assert!(
                    creator <= version,
                    "{} uses `{table}`, created by the later {creator}",
                    sql.lines().next().unwrap()
                );
            }
        }
    }
}

/// The same check against the real engine: a fresh store migrates to head
/// (every migration applied, none failed).
#[tokio::test]
async fn sqlite_migrates_an_empty_database_to_head() {
    let dir = tempfile::tempdir().unwrap();
    drop(Store::open(dir.path()).await.unwrap());
    let url = format!("sqlite://{}", dir.path().join("sun.db").display());
    let mut connection = sqlx::SqliteConnection::connect(&url).await.unwrap();
    let applied: Vec<i64> =
        sqlx::query_scalar("SELECT version FROM _sqlx_migrations WHERE success ORDER BY version")
            .fetch_all(&mut connection)
            .await
            .unwrap();
    let head: Vec<i64> = migrations().iter().map(|(version, _)| *version).collect();
    assert_eq!(applied, head);
}

/// Passkeys two members added with one credential before 20261024100000
/// (the race its unique index closes): the oldest stays, the later is gone.
#[tokio::test]
async fn duplicate_passkey_credentials_keep_the_oldest() {
    use sqlx::migrate::Migrate;
    let dir = tempfile::tempdir().unwrap();
    let url = format!("sqlite://{}?mode=rwc", dir.path().join("sun.db").display());
    let mut conn = sqlx::SqliteConnection::connect(&url).await.unwrap();
    conn.ensure_migrations_table().await.unwrap();
    let migrator = sqlx::migrate!("./migrations");
    for migration in migrator.iter().filter(|m| m.version < 20261024100000) {
        conn.apply(migration).await.unwrap();
    }
    for (id, data) in [
        ("01K00000000000000000000003", r#"{"id":[7],"counter":2}"#),
        ("01K00000000000000000000001", r#"{"id":[7],"counter":0}"#),
        ("01K00000000000000000000002", r#"{"id":[8],"counter":0}"#),
    ] {
        sqlx::query(
            "INSERT INTO member_factor (id, member_id, kind, label, data, created_at)
             VALUES ($1, '01K0000000000000000000000M', 'passkey', 'Key', $2,
                     '2026-10-01T00:00:00+00:00')",
        )
        .bind(id)
        .bind(data)
        .execute(&mut conn)
        .await
        .unwrap();
    }
    conn.close().await.unwrap();

    drop(Store::open(dir.path()).await.unwrap());
    let mut conn = sqlx::SqliteConnection::connect(&url).await.unwrap();
    let kept: Vec<String> = sqlx::query_scalar("SELECT id FROM member_factor ORDER BY id")
        .fetch_all(&mut conn)
        .await
        .unwrap();
    assert_eq!(
        kept,
        ["01K00000000000000000000001", "01K00000000000000000000002"]
    );
}

/// Cooldowns running when 20261106090000 lands carry over to
/// `account_cooldown`: a key's as itself, a CLI login's on every host of the
/// runtime's workspace (not on a host outside it); a runtime that isn't
/// cooling carries nothing.
#[tokio::test]
async fn running_cooldowns_move_to_their_accounts() {
    use sqlx::migrate::Migrate;
    let dir = tempfile::tempdir().unwrap();
    let url = format!("sqlite://{}?mode=rwc", dir.path().join("sun.db").display());
    let mut conn = sqlx::SqliteConnection::connect(&url).await.unwrap();
    conn.ensure_migrations_table().await.unwrap();
    let migrator = sqlx::migrate!("./migrations");
    for migration in migrator.iter().filter(|m| m.version < 20261106090000) {
        conn.apply(migration).await.unwrap();
    }
    let until = "2026-10-16T20:00:00+00:00";
    for sql in [
        "INSERT INTO host (id, name) VALUES ('H1', 'mac'), ('H2', 'vps'), ('H3', 'other')",
        "INSERT INTO workspace_host (workspace_id, host_id) VALUES
           ('00000000000000000000000001', 'H1'), ('00000000000000000000000001', 'H2')",
        "INSERT INTO runtime (id, workspace_id, name, adapter, source) VALUES
           ('R1', '00000000000000000000000001', 'claude-credits', 'claude-code', 'credits'),
           ('R2', '00000000000000000000000001', 'claude-code', 'claude-code', 'byo_runtime'),
           ('R3', '00000000000000000000000001', 'codex', 'codex', 'byo_runtime')",
        "INSERT INTO runtime_state (runtime_id, status, cooldown_until) VALUES
           ('R1', 'cooling_down', $1), ('R2', 'cooling_down', $1), ('R3', 'ready', NULL)",
    ] {
        sqlx::query(sql)
            .bind(until)
            .execute(&mut conn)
            .await
            .unwrap();
    }
    conn.close().await.unwrap();

    drop(Store::open(dir.path()).await.unwrap());
    let mut conn = sqlx::SqliteConnection::connect(&url).await.unwrap();
    let moved: Vec<(String, String)> =
        sqlx::query_as("SELECT account, until FROM account_cooldown ORDER BY account")
            .fetch_all(&mut conn)
            .await
            .unwrap();
    let at = |account: &str| (account.to_owned(), until.to_owned());
    assert_eq!(
        moved,
        [
            at("cred:R1"),
            at("login:H1:claude-code"),
            at("login:H2:claude-code")
        ]
    );
}

/// BLO-136 case 3 (cooldown across workspaces): a CLI-login cooldown of
/// workspace A that 20261106090000 carries over lands only on hosts that
/// serve A. Workspace B's own host (attached to B alone, never to A) must
/// not come out cooling: before the migration A's `runtime_state` row was
/// A's, and after it B's host entries must not be marked or blocked by it.
#[tokio::test]
async fn a_carried_login_cooldown_stays_on_its_own_workspaces_hosts() {
    use sqlx::migrate::Migrate;
    let dir = tempfile::tempdir().unwrap();
    let url = format!("sqlite://{}?mode=rwc", dir.path().join("sun.db").display());
    let mut conn = sqlx::SqliteConnection::connect(&url).await.unwrap();
    conn.ensure_migrations_table().await.unwrap();
    let migrator = sqlx::migrate!("./migrations");
    for migration in migrator.iter().filter(|m| m.version < 20261106090000) {
        conn.apply(migration).await.unwrap();
    }
    let until = "2099-01-01T00:00:00+00:00";
    for sql in [
        "INSERT INTO workspace (id, name, created_at, slug, issue_prefix) VALUES
           ('01K0000000000000000000000B', 'Beta', $1, 'beta', 'BETA')",
        "INSERT INTO host (id, name) VALUES ('HA', 'alpha-mac'), ('HB', 'beta-vps')",
        "INSERT INTO workspace_host (workspace_id, host_id) VALUES
           ('00000000000000000000000001', 'HA'), ('01K0000000000000000000000B', 'HB')",
        "INSERT INTO runtime (id, workspace_id, name, adapter, source) VALUES
           ('RA', '00000000000000000000000001', 'claude-code', 'claude-code', 'byo_runtime')",
        "INSERT INTO runtime_state (runtime_id, status, cooldown_until) VALUES
           ('RA', 'cooling_down', $1)",
    ] {
        sqlx::query(sql)
            .bind(until)
            .execute(&mut conn)
            .await
            .unwrap();
    }
    conn.close().await.unwrap();

    drop(Store::open(dir.path()).await.unwrap());
    let mut conn = sqlx::SqliteConnection::connect(&url).await.unwrap();
    let moved: Vec<(String,)> = sqlx::query_as("SELECT account FROM account_cooldown ORDER BY 1")
        .fetch_all(&mut conn)
        .await
        .unwrap();
    let moved: Vec<String> = moved.into_iter().map(|(a,)| a).collect();
    assert!(
        moved.contains(&"login:HA:claude-code".to_owned()),
        "{moved:?}"
    );
    assert!(
        !moved.contains(&"login:HB:claude-code".to_owned()),
        "workspace Beta's host cools on Main's cooldown: {moved:?}"
    );
}

/// Item 14: each inheriting agent gets the most restrictive `runtime_policy`
/// row among its chain's tools (resolved as placement does), an explicit
/// mode stays, an agent with no row keeps NULL (the host's default), and
/// the table is gone.
#[tokio::test]
async fn runtime_policy_fans_out_to_agents_most_restrictive_first() {
    use sqlx::migrate::Migrate;
    use sun_core::PermissionMode as M;
    use sun_store::AgentRepo;
    let dir = tempfile::tempdir().unwrap();
    let url = format!("sqlite://{}?mode=rwc", dir.path().join("sun.db").display());
    let mut conn = sqlx::SqliteConnection::connect(&url).await.unwrap();
    conn.ensure_migrations_table().await.unwrap();
    let migrator = sqlx::migrate!("./migrations");
    for migration in migrator.iter().filter(|m| m.version < 20261108090000) {
        conn.apply(migration).await.unwrap();
    }
    for sql in [
        "INSERT INTO runtime_policy (adapter, permission_mode) VALUES
           ('claude-code', 'accept_edits'), ('codex', 'bypass_permissions'),
           ('opencode', 'default'), ('gemini', 'plan')",
        "INSERT INTO runtime (id, workspace_id, name, adapter, source) VALUES
           ('R1', '00000000000000000000000001', 'my-key', 'opencode', 'byo_key'),
           ('R2', '00000000000000000000000001', 'team-key', 'codex', 'byo_key')",
    ] {
        sqlx::query(sql).execute(&mut conn).await.unwrap();
    }
    let agents = [
        // (chain, saved mode, mode after)
        (r#"["claude-code"]"#, None, Some(M::AcceptEdits)),
        (
            r#"["codex", {"tool": "claude-code"}]"#,
            None,
            Some(M::AcceptEdits),
        ),
        (r#"["my-key", "codex"]"#, None, Some(M::Default)),
        (
            r#"[{"tool": "opencode", "credential": "team-key"}]"#,
            None,
            Some(M::BypassPermissions),
        ),
        (
            r#"["codex", "gemini"]"#,
            Some("bypass_permissions"),
            Some(M::BypassPermissions),
        ),
        (r#"["market:gemini", "unknown-cli"]"#, None, None),
        (r#"["market:gemini", "gemini"]"#, None, Some(M::Plan)),
    ];
    let mut ids = Vec::new();
    for (i, (chain, mode, _)) in agents.iter().enumerate() {
        let id = sun_core::AgentId::generate().to_string();
        sqlx::query(
            "INSERT INTO agent (id, workspace_id, name, runtime_chain, permission_mode, created_at)
             VALUES ($1, '00000000000000000000000001', $2, $3, $4, '2026-10-17T10:00:00+00:00')",
        )
        .bind(&id)
        .bind(format!("agent-{i}"))
        .bind(chain)
        .bind(mode)
        .execute(&mut conn)
        .await
        .unwrap();
        ids.push(id);
    }
    conn.close().await.unwrap();

    let store = Store::open(dir.path()).await.unwrap();
    let mut tx = store.read().await.unwrap();
    for (id, (chain, _, after)) in ids.iter().zip(agents) {
        let agent = tx.get_agent(id.parse().unwrap()).await.unwrap();
        assert_eq!(agent.permission_mode, after, "{chain}");
    }
    drop(tx);
    let mut conn = sqlx::SqliteConnection::connect(&url).await.unwrap();
    let left: i64 =
        sqlx::query_scalar("SELECT count(*) FROM sqlite_master WHERE name = 'runtime_policy'")
            .fetch_one(&mut conn)
            .await
            .unwrap();
    assert_eq!(left, 0);
}
