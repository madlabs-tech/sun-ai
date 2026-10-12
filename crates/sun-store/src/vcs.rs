//! `vcs_connection`: a workspace's GitHub or GitLab connection.

use async_trait::async_trait;
use sqlx::Row;
use sqlx::sqlite::SqliteRow;
use sun_core::model::VcsConnection;
use sun_core::{Timestamp, VcsConnectionId, WorkspaceId};

use crate::repo::{parse, parse_ts, parse_ts_opt, ts};
use crate::{Result, StoreError, Tx};

/// **Repository** for `vcs_connection` rows.
#[async_trait]
pub trait VcsRepo {
    /// Inserts `connection`, or replaces the row with its id.
    async fn put_connection(&mut self, connection: &VcsConnection) -> Result<()>;
    /// One connection, or `NotFound`.
    async fn get_connection(&mut self, id: VcsConnectionId) -> Result<VcsConnection>;
    /// A workspace's connections, oldest first.
    async fn connections(&mut self, workspace: WorkspaceId) -> Result<Vec<VcsConnection>>;
    /// Every connection to GitHub App installation `installation_id`.
    async fn connections_of_installation(
        &mut self,
        installation_id: i64,
    ) -> Result<Vec<VcsConnection>>;
    /// Deletes a connection (the workspace's repos and mirrored PRs stay);
    /// false when there was none.
    async fn delete_connection(&mut self, id: VcsConnectionId) -> Result<bool>;
    /// When the connection's access token expires (GitLab OAuth).
    async fn set_token_expiry(&mut self, id: VcsConnectionId, at: Option<Timestamp>) -> Result<()>;
}

const COLUMNS: &str = "id, workspace_id, provider, instance_url, account_login, installation_id, \
     token_secret, refresh_secret, token_expires_at, oauth_redirect_uri, webhook_secret, connected_by, \
     created_at";

fn connection_from(row: &SqliteRow) -> Result<VcsConnection> {
    Ok(VcsConnection {
        id: parse(row.try_get("id")?)?,
        workspace_id: parse(row.try_get("workspace_id")?)?,
        provider: parse(row.try_get("provider")?)?,
        instance_url: row.try_get("instance_url")?,
        account_login: row.try_get("account_login")?,
        installation_id: row.try_get("installation_id")?,
        token_secret: row.try_get("token_secret")?,
        refresh_secret: row.try_get("refresh_secret")?,
        token_expires_at: parse_ts_opt(row.try_get("token_expires_at")?)?,
        oauth_redirect_uri: row.try_get("oauth_redirect_uri")?,
        webhook_secret: row.try_get("webhook_secret")?,
        connected_by: row.try_get("connected_by")?,
        created_at: parse_ts(row.try_get("created_at")?)?,
    })
}

#[async_trait]
impl VcsRepo for Tx {
    async fn put_connection(&mut self, c: &VcsConnection) -> Result<()> {
        sqlx::query(&format!(
            // An upsert, not `OR REPLACE`: a replace deletes the row first.
            "INSERT INTO vcs_connection ({COLUMNS})
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13)
             ON CONFLICT (id) DO UPDATE SET account_login = excluded.account_login,
               installation_id = excluded.installation_id, token_secret = excluded.token_secret,
               refresh_secret = excluded.refresh_secret,
               token_expires_at = excluded.token_expires_at,
               oauth_redirect_uri = excluded.oauth_redirect_uri,
               webhook_secret = excluded.webhook_secret, connected_by = excluded.connected_by"
        ))
        .bind(c.id.to_string())
        .bind(c.workspace_id.to_string())
        .bind(c.provider.as_str())
        .bind(&c.instance_url)
        .bind(&c.account_login)
        .bind(c.installation_id)
        .bind(&c.token_secret)
        .bind(&c.refresh_secret)
        .bind(c.token_expires_at.as_ref().map(ts))
        .bind(&c.oauth_redirect_uri)
        .bind(&c.webhook_secret)
        .bind(&c.connected_by)
        .bind(ts(&c.created_at))
        .execute(self.conn())
        .await?;
        Ok(())
    }

    async fn get_connection(&mut self, id: VcsConnectionId) -> Result<VcsConnection> {
        let row = sqlx::query(&format!(
            "SELECT {COLUMNS} FROM vcs_connection WHERE id = $1"
        ))
        .bind(id.to_string())
        .fetch_optional(self.conn())
        .await?
        .ok_or_else(|| StoreError::not_found("git connection", id))?;
        connection_from(&row)
    }

    async fn connections(&mut self, workspace: WorkspaceId) -> Result<Vec<VcsConnection>> {
        sqlx::query(&format!(
            "SELECT {COLUMNS} FROM vcs_connection WHERE workspace_id = $1 ORDER BY id"
        ))
        .bind(workspace.to_string())
        .fetch_all(self.conn())
        .await?
        .iter()
        .map(connection_from)
        .collect()
    }

    async fn connections_of_installation(
        &mut self,
        installation_id: i64,
    ) -> Result<Vec<VcsConnection>> {
        sqlx::query(&format!(
            "SELECT {COLUMNS} FROM vcs_connection
             WHERE provider = 'github' AND installation_id = $1 ORDER BY id"
        ))
        .bind(installation_id)
        .fetch_all(self.conn())
        .await?
        .iter()
        .map(connection_from)
        .collect()
    }

    async fn delete_connection(&mut self, id: VcsConnectionId) -> Result<bool> {
        let done = sqlx::query("DELETE FROM vcs_connection WHERE id = $1")
            .bind(id.to_string())
            .execute(self.conn())
            .await?;
        Ok(done.rows_affected() > 0)
    }

    async fn set_token_expiry(&mut self, id: VcsConnectionId, at: Option<Timestamp>) -> Result<()> {
        sqlx::query("UPDATE vcs_connection SET token_expires_at = $1 WHERE id = $2")
            .bind(at.as_ref().map(ts))
            .bind(id.to_string())
            .execute(self.conn())
            .await?;
        Ok(())
    }
}
