//! `host`: enrolled runtime hosts, deployment rows a workspace attaches
//! (`workspace_host`, [`crate::WorkspaceItem::Host`]), and `host_runtime`,
//! the `(host, tool)` rows projected from each host's capabilities report.
//! No host secret is stored here; the API derives and checks host keys itself.

use async_trait::async_trait;
use serde_json::Value;
use sqlx::Row;
use sqlx::sqlite::SqliteRow;
use sun_core::model::{Host, HostRuntime};
use sun_core::{HostId, HostRuntimeId, WorkspaceId};

use crate::repo::{json, parse, parse_opt, parse_ts, parse_ts_opt, ts, u32_at};
use crate::{Result, StoreError, Tx};

/// **Repository** for `host` rows.
#[async_trait]
pub trait HostRepo {
    /// Inserts an enrolled host.
    async fn insert_host(&mut self, host: &Host) -> Result<()>;
    /// The host, or `NotFound` (e.g. it was removed).
    async fn get_host(&mut self, id: HostId) -> Result<Host>;
    /// Every host, oldest first.
    async fn hosts(&mut self) -> Result<Vec<Host>>;
    /// The hosts attached to `workspace`, oldest first.
    async fn hosts_in(&mut self, workspace: WorkspaceId) -> Result<Vec<Host>>;
    /// Stores the capabilities the host reported in its `hello` or
    /// `capabilities` frame and rebuilds its `host_runtime` rows from them.
    async fn set_host_capabilities(
        &mut self,
        id: HostId,
        capabilities: &serde_json::Value,
    ) -> Result<()>;
    /// The `(host, tool)` rows of the hosts attached to `workspace` (`None`:
    /// every host) that the latest report still names, by host then tool.
    async fn host_runtimes(&mut self, workspace: Option<WorkspaceId>) -> Result<Vec<HostRuntime>>;
}

#[async_trait]
impl HostRepo for Tx {
    async fn insert_host(&mut self, host: &Host) -> Result<()> {
        sqlx::query(
            "INSERT INTO host (id, name, capabilities_json, enrolled_by) VALUES ($1, $2, $3, $4)",
        )
        .bind(host.id.to_string())
        .bind(&host.name)
        .bind(serde_json::to_string(&host.capabilities)?)
        .bind(host.enrolled_by.map(|m| m.to_string()))
        .execute(self.conn())
        .await?;
        project_host_runtimes(self.conn(), &host.id.to_string(), &host.capabilities).await
    }

    async fn get_host(&mut self, id: HostId) -> Result<Host> {
        let row = sqlx::query(&format!("SELECT {HOST_COLUMNS} FROM host WHERE id = $1"))
            .bind(id.to_string())
            .fetch_optional(self.conn())
            .await?
            .ok_or_else(|| StoreError::not_found("host", id))?;
        host_from(&row)
    }

    async fn hosts(&mut self) -> Result<Vec<Host>> {
        sqlx::query(&format!("SELECT {HOST_COLUMNS} FROM host ORDER BY id"))
            .fetch_all(self.conn())
            .await?
            .iter()
            .map(host_from)
            .collect()
    }

    async fn hosts_in(&mut self, workspace: WorkspaceId) -> Result<Vec<Host>> {
        sqlx::query(&format!(
            "SELECT {HOST_COLUMNS} FROM host \
             WHERE id IN (SELECT host_id FROM workspace_host WHERE workspace_id = $1) ORDER BY id"
        ))
        .bind(workspace.to_string())
        .fetch_all(self.conn())
        .await?
        .iter()
        .map(host_from)
        .collect()
    }

    async fn set_host_capabilities(
        &mut self,
        id: HostId,
        capabilities: &serde_json::Value,
    ) -> Result<()> {
        let done = sqlx::query("UPDATE host SET capabilities_json = $1 WHERE id = $2")
            .bind(serde_json::to_string(capabilities)?)
            .bind(id.to_string())
            .execute(self.conn())
            .await?;
        if done.rows_affected() == 0 {
            return Err(StoreError::not_found("host", id));
        }
        project_host_runtimes(self.conn(), &id.to_string(), capabilities).await
    }

    async fn host_runtimes(&mut self, workspace: Option<WorkspaceId>) -> Result<Vec<HostRuntime>> {
        let filter = match workspace {
            Some(_) => {
                "AND host_id IN (SELECT host_id FROM workspace_host WHERE workspace_id = $1)"
            }
            None => "AND $1 IS NULL",
        };
        sqlx::query(&format!(
            "SELECT {HOST_RUNTIME_COLUMNS} FROM host_runtime \
             WHERE removed_at IS NULL {filter} ORDER BY host_id, tool"
        ))
        .bind(workspace.map(|w| w.to_string()))
        .fetch_all(self.conn())
        .await?
        .iter()
        .map(host_runtime_from)
        .collect()
    }
}

const HOST_RUNTIME_COLUMNS: &str = "id, host_id, tool, family, version, installed, auth, detail, \
                                    max_sessions, probed_at, removed_at";

fn host_runtime_from(row: &SqliteRow) -> Result<HostRuntime> {
    Ok(HostRuntime {
        id: parse(row.try_get("id")?)?,
        host_id: parse(row.try_get("host_id")?)?,
        tool: row.try_get("tool")?,
        family: row.try_get("family")?,
        version: row.try_get("version")?,
        installed: row.try_get("installed")?,
        auth: row.try_get("auth")?,
        detail: row.try_get("detail")?,
        max_sessions: match row.try_get::<Option<i64>, _>("max_sessions")? {
            Some(_) => Some(u32_at(row, "max_sessions")?),
            None => None,
        },
        probed_at: parse_ts(row.try_get("probed_at")?)?,
        removed_at: parse_ts_opt(row.try_get("removed_at")?)?,
    })
}

/// Rebuilds a host's `host_runtime` rows from one capabilities report, in
/// the caller's transaction: every tool the report names is upserted
/// (`removed_at` cleared, same row id), every row it no longer names gets
/// `removed_at` now. Reads the report leniently (`runtimes[].adapter`,
/// `readiness`, and `family` / `max_sessions` when a host sends them), so an
/// older or empty report (`{}`) projects to what it says.
pub(crate) async fn project_host_runtimes(
    conn: &mut sqlx::SqliteConnection,
    host_id: &str,
    capabilities: &Value,
) -> Result<()> {
    let now = ts(&chrono::Utc::now());
    sqlx::query(
        "UPDATE host_runtime SET removed_at = $1 WHERE host_id = $2 AND removed_at IS NULL",
    )
    .bind(&now)
    .bind(host_id)
    .execute(&mut *conn)
    .await?;
    let runtimes = capabilities
        .get("runtimes")
        .and_then(Value::as_array)
        .into_iter()
        .flatten();
    for runtime in runtimes {
        let Some(tool) = runtime.get("adapter").and_then(Value::as_str) else {
            continue;
        };
        let readiness = runtime.get("readiness");
        let flag = |key: &str| readiness.and_then(|r| r.get(key)) == Some(&Value::Bool(true));
        let text = |key: &str| {
            readiness
                .and_then(|r| r.get(key))
                .and_then(Value::as_str)
                .map(str::to_owned)
        };
        let installed = flag("installed");
        let auth = match (installed, flag("logged_in")) {
            (false, _) => "unknown",
            (true, true) => "logged_in",
            (true, false) => "logged_out",
        };
        sqlx::query(
            "INSERT INTO host_runtime (id, host_id, tool, family, version, installed, auth, \
                                       detail, max_sessions, probed_at, removed_at) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, NULL) \
             ON CONFLICT (host_id, tool) DO UPDATE SET \
                 family = excluded.family, version = excluded.version, \
                 installed = excluded.installed, auth = excluded.auth, \
                 detail = excluded.detail, max_sessions = excluded.max_sessions, \
                 probed_at = excluded.probed_at, removed_at = NULL",
        )
        .bind(HostRuntimeId::generate().to_string())
        .bind(host_id)
        .bind(tool)
        .bind(runtime.get("family").and_then(Value::as_str))
        .bind(text("version"))
        .bind(installed)
        .bind(auth)
        .bind(text("detail"))
        .bind(runtime.get("max_sessions").and_then(Value::as_i64))
        .bind(&now)
        .execute(&mut *conn)
        .await?;
    }
    Ok(())
}

/// `20261104090100_host_runtime_backfill`: projects every host's stored
/// `capabilities_json` into `host_runtime` once, when the table is new.
pub(crate) async fn backfill_host_runtimes(conn: &mut sqlx::SqliteConnection) -> Result<()> {
    let hosts: Vec<(String, String)> =
        sqlx::query_as("SELECT id, capabilities_json FROM host ORDER BY id")
            .fetch_all(&mut *conn)
            .await?;
    for (host_id, capabilities) in hosts {
        let capabilities: Value = json(capabilities)?;
        project_host_runtimes(conn, &host_id, &capabilities).await?;
    }
    Ok(())
}

const HOST_COLUMNS: &str = "id, name, capabilities_json, enrolled_by";

fn host_from(row: &SqliteRow) -> Result<Host> {
    Ok(Host {
        id: parse(row.try_get("id")?)?,
        name: row.try_get("name")?,
        capabilities: json(row.try_get("capabilities_json")?)?,
        enrolled_by: parse_opt(row.try_get("enrolled_by")?)?,
    })
}
