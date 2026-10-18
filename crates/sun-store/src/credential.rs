//! `credential`: who pays for a session (TRD BLO-117 §7), a workspace's
//! API key or Sun credits, which chain entries name.

use async_trait::async_trait;
use sqlx::Row;
use sqlx::sqlite::SqliteRow;
use sun_core::model::Credential;
use sun_core::{ByoKey, CredentialId, WorkspaceId};

use crate::repo::parse;
use crate::{Result, StoreError, Tx};

/// **Repository** for `credential` rows. A credential belongs to one
/// workspace; names are unique per workspace and never change.
#[async_trait]
pub trait CredentialRepo {
    /// `workspace`'s credential named `name`, if any.
    async fn credential_by_name(
        &mut self,
        workspace: WorkspaceId,
        name: &str,
    ) -> Result<Option<Credential>>;
    /// The credentials of `workspace` (`None`: of every workspace), by name.
    async fn credentials(&mut self, workspace: Option<WorkspaceId>) -> Result<Vec<Credential>>;
    /// One credential, or `NotFound`.
    async fn get_credential(&mut self, id: CredentialId) -> Result<Credential>;
    /// Stores a credential (validated by the caller); `false` when its
    /// workspace has the name.
    async fn insert_credential(&mut self, credential: &Credential) -> Result<bool>;
    /// Replaces the source, tool and key settings of the credential named
    /// `credential.name` in `credential.workspace_id`; `false` when there is
    /// none.
    async fn update_credential(&mut self, credential: &Credential) -> Result<bool>;
    /// Deletes `workspace`'s credential `name` with its cooldown and credits
    /// model; its steps keep running history without the link. `None` when
    /// there is none.
    async fn delete_credential(
        &mut self,
        workspace: WorkspaceId,
        name: &str,
    ) -> Result<Option<Credential>>;
}

const CREDENTIAL_COLUMNS: &str = "id, workspace_id, name, family, source, endpoint, \
     default_model, key_secret, default_tool, created_by";

#[async_trait]
impl CredentialRepo for Tx {
    async fn credential_by_name(
        &mut self,
        workspace: WorkspaceId,
        name: &str,
    ) -> Result<Option<Credential>> {
        sqlx::query(&format!(
            "SELECT {CREDENTIAL_COLUMNS} FROM credential WHERE workspace_id = $1 AND name = $2"
        ))
        .bind(workspace.to_string())
        .bind(name)
        .fetch_optional(self.conn())
        .await?
        .map(|row| credential_from(&row))
        .transpose()
    }

    async fn credentials(&mut self, workspace: Option<WorkspaceId>) -> Result<Vec<Credential>> {
        let rows = sqlx::query(&format!(
            "SELECT {CREDENTIAL_COLUMNS} FROM credential WHERE $1 IS NULL OR workspace_id = $1 \
             ORDER BY name, workspace_id"
        ))
        .bind(workspace.map(|w| w.to_string()))
        .fetch_all(self.conn())
        .await?;
        rows.iter().map(credential_from).collect()
    }

    async fn get_credential(&mut self, id: CredentialId) -> Result<Credential> {
        let row = sqlx::query(&format!(
            "SELECT {CREDENTIAL_COLUMNS} FROM credential WHERE id = $1"
        ))
        .bind(id.to_string())
        .fetch_optional(self.conn())
        .await?
        .ok_or_else(|| StoreError::not_found("credential", id))?;
        credential_from(&row)
    }

    async fn insert_credential(&mut self, c: &Credential) -> Result<bool> {
        let byo = c.byo_key.as_ref();
        let done = sqlx::query(&format!(
            "INSERT INTO credential ({CREDENTIAL_COLUMNS}) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10) \
             ON CONFLICT (workspace_id, name) DO NOTHING"
        ))
        .bind(c.id.to_string())
        .bind(c.workspace_id.to_string())
        .bind(c.name.as_str())
        .bind(c.family().as_str())
        .bind(c.source.as_str())
        .bind(byo.map(|b| b.endpoint.as_str()).filter(|e| !e.is_empty()))
        .bind(byo.map(|b| b.model.as_str()))
        .bind(byo.and_then(|b| b.key_secret.as_deref()))
        .bind(c.default_tool.as_str())
        .bind(c.created_by.map(|m| m.to_string()))
        .execute(self.conn())
        .await?;
        Ok(done.rows_affected() > 0)
    }

    async fn update_credential(&mut self, c: &Credential) -> Result<bool> {
        let byo = c.byo_key.as_ref();
        let done = sqlx::query(
            "UPDATE credential SET family = $1, source = $2, endpoint = $3, default_model = $4, \
             key_secret = $5, default_tool = $6 WHERE workspace_id = $7 AND name = $8",
        )
        .bind(c.family().as_str())
        .bind(c.source.as_str())
        .bind(byo.map(|b| b.endpoint.as_str()).filter(|e| !e.is_empty()))
        .bind(byo.map(|b| b.model.as_str()))
        .bind(byo.and_then(|b| b.key_secret.as_deref()))
        .bind(c.default_tool.as_str())
        .bind(c.workspace_id.to_string())
        .bind(c.name.as_str())
        .execute(self.conn())
        .await?;
        Ok(done.rows_affected() > 0)
    }

    async fn delete_credential(
        &mut self,
        workspace: WorkspaceId,
        name: &str,
    ) -> Result<Option<Credential>> {
        let Some(credential) = self.credential_by_name(workspace, name).await? else {
            return Ok(None);
        };
        let id = credential.id.to_string();
        sqlx::query("DELETE FROM account_cooldown WHERE account = 'cred:' || $1")
            .bind(&id)
            .execute(self.conn())
            .await?;
        sqlx::query("DELETE FROM kernel_setting WHERE key = 'credits_model:' || $1")
            .bind(&id)
            .execute(self.conn())
            .await?;
        sqlx::query("DELETE FROM credential WHERE id = $1")
            .bind(&id)
            .execute(self.conn())
            .await?;
        Ok(Some(credential))
    }
}

/// A row back into a [`Credential`]: a `byo_key` one gets its key settings
/// as a [`ByoKey`] (no endpoint: the provider's own).
fn credential_from(row: &SqliteRow) -> Result<Credential> {
    let source: sun_core::model::RuntimeSource = parse(row.try_get("source")?)?;
    let model: Option<String> = row.try_get("default_model")?;
    let byo_key = match (source, model) {
        (sun_core::model::RuntimeSource::ByoKey, Some(model)) => Some(ByoKey {
            kind: parse(row.try_get("family")?)?,
            endpoint: row
                .try_get::<Option<String>, _>("endpoint")?
                .unwrap_or_default(),
            model,
            key_secret: row.try_get("key_secret")?,
        }),
        _ => None,
    };
    let created_by: Option<String> = row.try_get("created_by")?;
    Ok(Credential {
        id: parse(row.try_get("id")?)?,
        workspace_id: parse(row.try_get("workspace_id")?)?,
        name: row.try_get("name")?,
        source,
        default_tool: row.try_get("default_tool")?,
        byo_key,
        created_by: created_by.map(parse).transpose()?,
    })
}
