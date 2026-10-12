//! Hosts and catalog installs belong to workspaces
//! (`20261103090000`..`20261103090200`): checked on a copy of an old-schema
//! database, then the links keep each workspace's own.
#![allow(clippy::unwrap_used)]

use sqlx::migrate::Migrate;
use sqlx::{Connection, SqliteConnection};
use sun_core::model::Host;
use sun_core::{HostId, MemberId, WorkspaceId};
use sun_store::{HostRepo, Store, WorkspaceItem, WorkspaceRepo};

/// The first migration that attaches hosts to workspaces.
const M5: i64 = 20261103090000;

const OPS: &str = "01K0000000000000000000000W";
const HOST: &str = "01K00000000000000000000H01";
const SELLER: &str = "01K00000000000000000000H02";
const AT: &str = "2026-10-30T00:00:00+00:00";

#[tokio::test]
async fn hosts_and_catalog_installs_go_to_main_on_an_old_database_copy() {
    let dir = tempfile::tempdir().unwrap();
    let old = dir.path().join("old");
    std::fs::create_dir_all(&old).unwrap();
    let url = format!("sqlite://{}?mode=rwc", old.join("sun.db").display());
    let mut conn = SqliteConnection::connect(&url).await.unwrap();
    conn.ensure_migrations_table().await.unwrap();
    for migration in sqlx::migrate!("./migrations")
        .iter()
        .filter(|m| m.version < M5)
    {
        conn.apply(migration).await.unwrap();
    }
    for sql in [
        format!(
            "INSERT INTO workspace (id, name, slug, issue_prefix, created_at) \
             VALUES ('{OPS}', 'Ops', 'ops', 'OPS', '{AT}')"
        ),
        format!("INSERT INTO host (id, name) VALUES ('{HOST}', 'mac'), ('{SELLER}', 'seller')"),
        // A leased seller host serves only its task.
        format!(
            "INSERT INTO kernel_setting (key, value_json) VALUES ('host_market.{SELLER}', '{{}}')"
        ),
    ] {
        sqlx::query(&sql).execute(&mut conn).await.unwrap();
    }
    conn.close().await.unwrap();
    let data = dir.path().join("copy");
    std::fs::create_dir_all(data.join("catalog")).unwrap();
    std::fs::copy(old.join("sun.db"), data.join("sun.db")).unwrap();
    std::fs::write(
        data.join("catalog/installed.json"),
        r#"{"tdd": "1.0.0", "github": "2.0.0"}"#,
    )
    .unwrap();

    let store = Store::open(&data).await.unwrap();
    let (main, ops) = (WorkspaceId::MAIN, OPS.parse::<WorkspaceId>().unwrap());
    let (host, seller) = (
        HOST.parse::<HostId>().unwrap(),
        SELLER.parse::<HostId>().unwrap(),
    );
    let mut tx = store.begin().await.unwrap();
    let ids = |hosts: Vec<Host>| hosts.into_iter().map(|h| h.id).collect::<Vec<_>>();
    assert_eq!(ids(tx.hosts_in(main).await.unwrap()), [host]);
    assert!(tx.hosts_in(ops).await.unwrap().is_empty());
    assert!(
        tx.workspaces_of(WorkspaceItem::Host(seller))
            .await
            .unwrap()
            .is_empty()
    );
    assert_eq!(tx.get_host(host).await.unwrap().enrolled_by, None);
    assert_eq!(tx.catalog_installs(main).await.unwrap(), ["github", "tdd"]);
    assert!(tx.catalog_installs(ops).await.unwrap().is_empty());

    // A host enrolled in Ops by a member, attached to both, then detached.
    let by = MemberId::generate();
    let new = Host {
        id: HostId::generate(),
        name: "box".into(),
        capabilities: serde_json::json!({}),
        enrolled_by: Some(by),
    };
    tx.insert_host(&new).await.unwrap();
    tx.link(ops, WorkspaceItem::Host(new.id)).await.unwrap();
    tx.link(main, WorkspaceItem::Host(new.id)).await.unwrap();
    assert_eq!(tx.get_host(new.id).await.unwrap().enrolled_by, Some(by));
    assert_eq!(ids(tx.hosts_in(ops).await.unwrap()), [new.id]);
    assert_eq!(ids(tx.hosts_in(main).await.unwrap()), [host, new.id]);
    assert!(tx.unlink(main, WorkspaceItem::Host(new.id)).await.unwrap());
    assert!(!tx.unlink(main, WorkspaceItem::Host(new.id)).await.unwrap());
    assert_eq!(
        tx.workspaces_of(WorkspaceItem::Host(new.id)).await.unwrap(),
        [ops]
    );

    // Catalog installs: Ops' own; deleting Ops removes its links only.
    tx.link(ops, WorkspaceItem::CatalogInstall("tdd"))
        .await
        .unwrap();
    assert_eq!(tx.catalog_installs(ops).await.unwrap(), ["tdd"]);
    assert_eq!(
        tx.workspaces_of(WorkspaceItem::CatalogInstall("tdd"))
            .await
            .unwrap(),
        [main, ops]
    );
    tx.delete_workspace(ops).await.unwrap();
    assert!(
        tx.workspaces_of(WorkspaceItem::Host(new.id))
            .await
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        tx.workspaces_of(WorkspaceItem::CatalogInstall("tdd"))
            .await
            .unwrap(),
        [main]
    );
    assert!(tx.get_host(new.id).await.is_ok(), "the host row stays");
    tx.commit().await.unwrap();
}

/// A new data dir has no install file and no hosts: nothing to seed.
#[tokio::test]
async fn a_new_store_has_no_catalog_installs() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).await.unwrap();
    let mut tx = store.read().await.unwrap();
    assert!(
        tx.catalog_installs(WorkspaceId::MAIN)
            .await
            .unwrap()
            .is_empty()
    );
}
