//! A data dir written by the last commit before Postgres was dropped
//! (ec08176, `sqlx::Any`; `tests/fixtures/pre_t2/make.sh` rebuilds it) opens
//! on today's typed SQLite store with every row and blob intact.
#![allow(clippy::unwrap_used)]

use std::path::Path;

use serde_json::Value;
use sqlx::Connection;
use sun_core::model::{Role, SecretScope};
use sun_core::{BlobHash, TaskStatus, WorkspaceId};
use sun_store::{
    ArtifactRepo, FactorRepo, MemberRepo, MembershipRepo, SecretRepo, Store, TaskRepo,
    WorkspaceRepo,
};

fn copy_dir(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for entry in std::fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let target = to.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_dir(&entry.path(), &target);
        } else {
            std::fs::copy(entry.path(), target).unwrap();
        }
    }
}

/// Unpacks the fixture into `data` (db, blobs and its test-only `master.key`;
/// key files are git-ignored) and returns its `expected.json`.
fn unpack_pre_t2(data: &Path) -> Value {
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/pre_t2");
    let expected: Value =
        serde_json::from_slice(&std::fs::read(fixture.join("expected.json")).unwrap()).unwrap();
    let db = zstd::decode_all(
        std::fs::read(fixture.join("sun.db.zst"))
            .unwrap()
            .as_slice(),
    );
    std::fs::write(data.join("sun.db"), db.unwrap()).unwrap();
    copy_dir(&fixture.join("blobs"), &data.join("blobs"));
    {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(data.join("master.key"))
            .unwrap();
        file.write_all(expected["master_key"].as_str().unwrap().as_bytes())
            .unwrap();
    }
    expected
}

#[tokio::test]
async fn a_pre_t2_data_dir_opens_with_tasks_members_roles_factors_secrets_and_blobs() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path();
    let expected = unpack_pre_t2(data);
    let id = |key: &str| expected[key].as_str().unwrap().to_owned();

    let store = Store::open(data).await.unwrap();
    let mut tx = store.read().await.unwrap();

    let task = tx.get_task(id("task").parse().unwrap()).await.unwrap();
    assert_eq!(task.title, "Written before T2");
    assert_eq!(task.status, TaskStatus::Done);
    let artifacts = tx.artifacts_of(task.id).await.unwrap();
    let notes = artifacts.iter().find(|a| a.name == "notes.md").unwrap();
    let bytes = store.blobs().get(&notes.blob).await.unwrap();
    assert_eq!(bytes, b"# Notes from before T2\n");
    // Every blob the old ecosystem wrote reads back verified.
    let mut blobs = 0;
    for shard in std::fs::read_dir(data.join("blobs")).unwrap() {
        for file in std::fs::read_dir(shard.unwrap().path()).unwrap() {
            let name = file.unwrap().file_name().into_string().unwrap();
            let hash = BlobHash::new(name.trim_end_matches(".zst")).unwrap();
            store.blobs().get(&hash).await.unwrap();
            blobs += 1;
        }
    }
    assert_eq!(blobs, expected["blobs"].as_u64().unwrap());

    let member = tx
        .members()
        .await
        .unwrap()
        .into_iter()
        .find(|m| m.id == id("member").parse().unwrap())
        .unwrap();
    assert_eq!(member.username, "ana");
    // Ana was in a custom group that could run: a member of Main, where
    // every member lands; she had a password, so her email counts as verified.
    assert_eq!(
        tx.role_of(WorkspaceId::MAIN, member.id).await.unwrap(),
        Some(Role::Member)
    );
    assert!(member.email_verified_at.is_some());
    assert_eq!(
        tx.get_workspace(WorkspaceId::MAIN).await.unwrap().name,
        "Main"
    );

    // The sealed values open with the old key: the 2-step secret and the
    // ecosystem secret.
    let totp = tx.totp_secrets(member.id).await.unwrap();
    assert_eq!(totp.len(), 1);
    assert_eq!(totp[0].0, id("factor").parse().unwrap());
    assert_eq!(totp[0].1, expected["totp"].as_str().unwrap());
    assert!(totp[0].2.is_some(), "the authenticator was confirmed");
    let ecosystem = SecretScope::Ecosystem("00000000000000000000000000".parse().unwrap());
    // A person's ecosystem secret moved to Main (M4).
    let main = SecretScope::Workspace(WorkspaceId::MAIN);
    assert_eq!(
        tx.secret_value(&main, "API_TOKEN").await.unwrap(),
        "pre-t2-secret-value-42"
    );
    drop(tx);

    // The fixture predates the workspace rename: that migration ran after a
    // backup, and writes still work.
    assert!(data.join("backups").exists());
    let mut tx = store.begin().await.unwrap();
    tx.set_secret(&ecosystem, "AFTER_T2", "written-after-the-upgrade")
        .await
        .unwrap();
    tx.commit().await.unwrap();
    drop(store);

    // The old `workspace` secret was re-sealed as `ecosystem` by the data
    // step, and `secret_to_reseal` and `workspace.logo_blob` are gone.
    let url = format!("sqlite://{}", data.join("sun.db").display());
    let rows = || async {
        let mut conn = sqlx::SqliteConnection::connect(&url).await.unwrap();
        let rows: Vec<(String, String, Vec<u8>)> =
            sqlx::query_as("SELECT name, scope_kind, sealed FROM secret ORDER BY name")
                .fetch_all(&mut conn)
                .await
                .unwrap();
        let pending: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM sqlite_schema WHERE name = 'secret_to_reseal'
             UNION ALL SELECT 0 LIMIT 1",
        )
        .fetch_one(&mut conn)
        .await
        .unwrap();
        let logo_blob: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM pragma_table_info('workspace') WHERE name = 'logo_blob'",
        )
        .fetch_one(&mut conn)
        .await
        .unwrap();
        assert_eq!(logo_blob, 0);
        conn.close().await.unwrap();
        (rows, pending)
    };
    let (after_upgrade, pending) = rows().await;
    assert_eq!(pending, 0);
    let kinds: Vec<&str> = after_upgrade.iter().map(|r| r.1.as_str()).collect();
    assert_eq!(kinds, ["ecosystem", "workspace"]);

    // A secret stored as `workspace` after the migration (a per-workspace
    // secret, sealed with its own AAD) is never re-sealed.
    let key = sun_store::MasterKey::load(data).unwrap();
    let ws = WorkspaceId::MAIN.to_string();
    let sealed = key
        .seal(
            format!("workspace\0{ws}\0WS_TOKEN").as_bytes(),
            b"per-workspace-value",
        )
        .unwrap();
    {
        let mut conn = sqlx::SqliteConnection::connect(&url).await.unwrap();
        sqlx::query(
            "INSERT INTO secret (id, scope_kind, scope_id, name, sealed, updated_at)
             VALUES ('01JWS0000000000000000000WS', 'workspace', $1, 'WS_TOKEN', $2, $3)",
        )
        .bind(&ws)
        .bind(&sealed)
        .bind(chrono::Utc::now().to_rfc3339())
        .execute(&mut conn)
        .await
        .unwrap();
        conn.close().await.unwrap();
    }
    let (before, _) = rows().await;

    // Two more opens: nothing left to re-seal, every value still opens, and
    // no row changes, the `workspace` one included.
    for _ in 0..2 {
        let store = Store::open(data).await.unwrap();
        let mut tx = store.read().await.unwrap();
        for (scope, name, value) in [
            (&main, "API_TOKEN", "pre-t2-secret-value-42"),
            (&ecosystem, "AFTER_T2", "written-after-the-upgrade"),
        ] {
            assert_eq!(tx.secret_value(scope, name).await.unwrap(), value);
        }
        drop(tx);
        drop(store);
        assert_eq!(rows().await, (before.clone(), 0));
    }
    let ws_row = before.iter().find(|r| r.0 == "WS_TOKEN").unwrap();
    assert_eq!((ws_row.1.as_str(), &ws_row.2), ("workspace", &sealed));
}

/// A data dir from before the master-key canary gets one on its first open,
/// and from then on refuses another key even once nothing else is sealed.
#[tokio::test]
async fn a_pre_canary_data_dir_gets_the_canary_and_then_refuses_a_wrong_key() {
    use sun_store::{SettingRepo, StoreError};
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path();
    unpack_pre_t2(data);
    let main = SecretScope::Workspace(WorkspaceId::MAIN);
    {
        let store = Store::open(data).await.unwrap();
        let mut tx = store.begin().await.unwrap();
        assert!(tx.setting("master_key_canary").await.unwrap().is_some());
        // Only the canary is left to tell the keys apart.
        assert!(tx.delete_secret(&main, "API_TOKEN").await.unwrap());
        tx.commit().await.unwrap();
    }
    std::fs::write(data.join("master.key"), "ab".repeat(32)).unwrap();
    let err = Store::open(data).await.unwrap_err();
    assert!(matches!(err, StoreError::Crypto(_)), "{err}");
    assert!(err.to_string().contains("first opened with"), "{err}");
}
