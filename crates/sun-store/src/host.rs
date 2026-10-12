//! `host`: enrolled runtime hosts, deployment rows a workspace attaches
//! (`workspace_host`, [`crate::WorkspaceItem::Host`]). No host secret is
//! stored here; the API derives and checks host keys itself.

use async_trait::async_trait;
use sqlx::Row;
use sqlx::sqlite::SqliteRow;
use sun_core::model::Host;
use sun_core::{HostId, WorkspaceId};

use crate::repo::{json, parse, parse_opt};
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
    /// Stores the capabilities the host reported in its `hello`.
    async fn set_host_capabilities(
        &mut self,
        id: HostId,
        capabilities: &serde_json::Value,
    ) -> Result<()>;
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
        Ok(())
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
        Ok(())
    }
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
