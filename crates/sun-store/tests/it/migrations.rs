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
