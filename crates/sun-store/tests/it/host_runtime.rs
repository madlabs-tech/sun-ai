//! `host_runtime`, the `(host, tool)` rows projected from a host's
//! capabilities report (`20261104090000`, backfilled by `20261104090100`):
//! every `hello` / `capabilities` write rebuilds them, a tool gone from a
//! later report keeps its row with `removed_at`, and comes back on the same
//! row; the backfill fills the table from stored reports on an old-schema
//! database copy.
#![allow(clippy::unwrap_used)]

use std::path::Path;

use serde_json::{Value, json};
use sqlx::migrate::Migrate;
use sqlx::{Connection, Row, SqliteConnection};
use sun_core::model::{Host, HostRuntime};
use sun_core::{HostId, WorkspaceId};
use sun_store::{HostRepo, Store, WorkspaceItem, WorkspaceRepo};

/// The migration that creates `host_runtime`.
const HOST_RUNTIME: i64 = 20261104090000;

const OPS: &str = "01K0000000000000000000000W";
const HOST_A: &str = "01K00000000000000000000H01";
const HOST_B: &str = "01K00000000000000000000H02";
const AT: &str = "2026-10-30T00:00:00+00:00";

fn runtime(adapter: &str, installed: bool, logged_in: bool) -> Value {
    json!({
        "adapter": adapter,
        "readiness": {
            "installed": installed,
            "version": if installed { Some("1.2.3") } else { None },
            "logged_in": logged_in,
            "detail": if logged_in { None } else { Some("run `login`") },
        },
        "effort_options": [],
        "models": [],
    })
}

fn report(runtimes: Vec<Value>) -> Value {
    json!({ "os": "linux", "arch": "x86_64", "cpus": 2, "runtimes": runtimes })
}

async fn rows(store: &Store, workspace: Option<WorkspaceId>) -> Vec<HostRuntime> {
    store
        .read()
        .await
        .unwrap()
        .host_runtimes(workspace)
        .await
        .unwrap()
}

/// Every row of a host, removed ones too: `(tool, id, removed)`.
async fn history(data: &Path, host: HostId) -> Vec<(String, String, bool)> {
    let url = format!("sqlite://{}?mode=ro", data.join("sun.db").display());
    let mut conn = SqliteConnection::connect(&url).await.unwrap();
    let mut out = Vec::new();
    for row in sqlx::query(
        "SELECT tool, id, removed_at IS NOT NULL AS removed FROM host_runtime \
         WHERE host_id = $1 ORDER BY tool",
    )
    .bind(host.to_string())
    .fetch_all(&mut conn)
    .await
    .unwrap()
    {
        out.push((
            row.get::<String, _>("tool"),
            row.get::<String, _>("id"),
            row.get::<bool, _>("removed"),
        ));
    }
    out
}

#[tokio::test]
async fn hello_then_a_smaller_capabilities_report_marks_the_missing_tool_removed_and_back() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).await.unwrap();
    let host = Host {
        id: HostId::generate(),
        name: "box".into(),
        capabilities: report(vec![
            runtime("claude-code", true, true),
            runtime("codex", true, false),
        ]),
        enrolled_by: None,
    };
    let ops = OPS.parse::<WorkspaceId>().unwrap();

    // `hello` with 2 tools: 2 rows, auth from the readiness flags.
    let mut tx = store.begin().await.unwrap();
    tx.insert_host(&host).await.unwrap();
    tx.link(WorkspaceId::MAIN, WorkspaceItem::Host(host.id))
        .await
        .unwrap();
    tx.commit().await.unwrap();
    let first = rows(&store, None).await;
    assert_eq!(
        first
            .iter()
            .map(|r| (
                r.tool.as_str(),
                r.installed,
                r.auth.as_str(),
                r.version.as_deref()
            ))
            .collect::<Vec<_>>(),
        [
            ("claude-code", true, "logged_in", Some("1.2.3")),
            ("codex", true, "logged_out", Some("1.2.3")),
        ]
    );
    assert!(
        first
            .iter()
            .all(|r| r.host_id == host.id && r.removed_at.is_none())
    );
    assert_eq!(first[1].detail.as_deref(), Some("run `login`"));
    let codex_id = first[1].id;
    // Scoped to the workspaces the host is attached to.
    assert_eq!(rows(&store, Some(WorkspaceId::MAIN)).await.len(), 2);
    assert!(rows(&store, Some(ops)).await.is_empty());

    // The next `capabilities` names 1 tool: the other row gets `removed_at`.
    let mut tx = store.begin().await.unwrap();
    tx.set_host_capabilities(host.id, &report(vec![runtime("claude-code", true, true)]))
        .await
        .unwrap();
    tx.commit().await.unwrap();
    let live: Vec<_> = rows(&store, None)
        .await
        .into_iter()
        .map(|r| r.tool)
        .collect();
    assert_eq!(live, ["claude-code"]);
    assert_eq!(
        history(dir.path(), host.id).await,
        [
            ("claude-code".into(), first[0].id.to_string(), false),
            ("codex".into(), codex_id.to_string(), true),
        ]
    );

    // The tool returns (now not installed): `removed_at` cleared, same row.
    let mut tx = store.begin().await.unwrap();
    tx.set_host_capabilities(
        host.id,
        &report(vec![
            runtime("claude-code", true, true),
            runtime("codex", false, false),
        ]),
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    let back = rows(&store, None).await;
    assert_eq!(back.len(), 2);
    assert_eq!(back[1].id, codex_id);
    assert_eq!(
        (back[1].installed, back[1].auth.as_str()),
        (false, "unknown")
    );
    assert!(back[1].probed_at >= first[1].probed_at);
    assert!(history(dir.path(), host.id).await.iter().all(|r| !r.2));

    // The raw report stays what the host sent.
    let stored = store
        .read()
        .await
        .unwrap()
        .get_host(host.id)
        .await
        .unwrap()
        .capabilities;
    assert_eq!(stored["runtimes"].as_array().unwrap().len(), 2);
}

#[tokio::test]
async fn the_backfill_projects_every_stored_report_on_an_old_database_copy() {
    let dir = tempfile::tempdir().unwrap();
    let old = dir.path().join("old");
    std::fs::create_dir_all(&old).unwrap();
    let url = format!("sqlite://{}?mode=rwc", old.join("sun.db").display());
    let mut conn = SqliteConnection::connect(&url).await.unwrap();
    conn.ensure_migrations_table().await.unwrap();
    for migration in sqlx::migrate!("./migrations")
        .iter()
        .filter(|m| m.version < HOST_RUNTIME)
    {
        conn.apply(migration).await.unwrap();
    }
    let a = report(vec![
        runtime("claude-code", true, true),
        runtime("opencode", false, false),
    ]);
    for sql in [
        format!(
            "INSERT INTO workspace (id, name, slug, issue_prefix, created_at) \
             VALUES ('{OPS}', 'Ops', 'ops', 'OPS', '{AT}')"
        ),
        format!(
            "INSERT INTO host (id, name, capabilities_json) VALUES \
             ('{HOST_A}', 'mac', '{a}'), ('{HOST_B}', 'fresh', '{{}}')"
        ),
        format!("INSERT INTO workspace_host (workspace_id, host_id) VALUES ('{OPS}', '{HOST_A}')"),
    ] {
        sqlx::query(&sql).execute(&mut conn).await.unwrap();
    }
    conn.close().await.unwrap();
    let data = dir.path().join("copy");
    std::fs::create_dir_all(&data).unwrap();
    std::fs::copy(old.join("sun.db"), data.join("sun.db")).unwrap();

    let store = Store::open(&data).await.unwrap();
    let (host_a, ops) = (
        HOST_A.parse::<HostId>().unwrap(),
        OPS.parse::<WorkspaceId>().unwrap(),
    );
    let all = rows(&store, None).await;
    assert_eq!(
        all.iter()
            .map(|r| (r.host_id, r.tool.as_str(), r.installed, r.auth.as_str()))
            .collect::<Vec<_>>(),
        [
            (host_a, "claude-code", true, "logged_in"),
            (host_a, "opencode", false, "unknown"),
        ]
    );
    assert_eq!(rows(&store, Some(ops)).await.len(), 2);
    assert!(rows(&store, Some(WorkspaceId::MAIN)).await.is_empty());
    // Nothing else changed: the raw reports are as stored.
    let mut read = store.read().await.unwrap();
    assert_eq!(read.get_host(host_a).await.unwrap().capabilities, a);
    assert_eq!(
        read.get_host(HOST_B.parse().unwrap())
            .await
            .unwrap()
            .capabilities,
        json!({})
    );
    drop(read);
    drop(store);

    // A second open runs no migration and changes no row.
    let store = Store::open(&data).await.unwrap();
    assert_eq!(rows(&store, None).await, all);
}
