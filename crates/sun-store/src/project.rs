//! `project`: named groups of one workspace's tasks.

use async_trait::async_trait;
use sqlx::Row;
use sqlx::sqlite::SqliteRow;
use sun_core::model::Project;
use sun_core::{ProjectId, WorkspaceId};

use crate::repo::{parse, parse_ts, ts};
use crate::{Result, StoreError, Tx};

/// **Repository** for `project` rows. Fields are checked with
/// [`Project`]'s `clean_*` helpers by the caller; here names must be unique
/// in their workspace ignoring case.
#[async_trait]
pub trait ProjectRepo {
    /// Inserts a project; `InvalidInput` when its workspace has the name.
    async fn insert_project(&mut self, project: &Project) -> Result<()>;
    /// One project, or `NotFound`.
    async fn get_project(&mut self, id: ProjectId) -> Result<Project>;
    /// A workspace's projects, by name.
    async fn projects_of_workspace(&mut self, workspace: WorkspaceId) -> Result<Vec<Project>>;
    /// Saves a project's name and description; `NotFound` or `InvalidInput`
    /// (name taken).
    async fn update_project(&mut self, project: &Project) -> Result<()>;
    /// Deletes a project; `NotFound` when missing.
    async fn delete_project(&mut self, id: ProjectId) -> Result<()>;
}

const COLUMNS: &str = "id, workspace_id, name, description, created_at";

#[async_trait]
impl ProjectRepo for Tx {
    async fn insert_project(&mut self, project: &Project) -> Result<()> {
        name_free(self, project).await?;
        sqlx::query(&format!(
            "INSERT INTO project ({COLUMNS}) VALUES ($1, $2, $3, $4, $5)"
        ))
        .bind(project.id.to_string())
        .bind(project.workspace_id.to_string())
        .bind(&project.name)
        .bind(&project.description)
        .bind(ts(&project.created_at))
        .execute(self.conn())
        .await?;
        Ok(())
    }

    async fn get_project(&mut self, id: ProjectId) -> Result<Project> {
        let row = sqlx::query(&format!("SELECT {COLUMNS} FROM project WHERE id = $1"))
            .bind(id.to_string())
            .fetch_optional(self.conn())
            .await?
            .ok_or_else(|| StoreError::not_found("project", id))?;
        project_from(&row)
    }

    async fn projects_of_workspace(&mut self, workspace: WorkspaceId) -> Result<Vec<Project>> {
        sqlx::query(&format!(
            "SELECT {COLUMNS} FROM project WHERE workspace_id = $1 ORDER BY lower(name)"
        ))
        .bind(workspace.to_string())
        .fetch_all(self.conn())
        .await?
        .iter()
        .map(project_from)
        .collect()
    }

    async fn update_project(&mut self, project: &Project) -> Result<()> {
        name_free(self, project).await?;
        let done = sqlx::query("UPDATE project SET name = $1, description = $2 WHERE id = $3")
            .bind(&project.name)
            .bind(&project.description)
            .bind(project.id.to_string())
            .execute(self.conn())
            .await?;
        if done.rows_affected() == 0 {
            return Err(StoreError::not_found("project", project.id));
        }
        Ok(())
    }

    async fn delete_project(&mut self, id: ProjectId) -> Result<()> {
        let done = sqlx::query("DELETE FROM project WHERE id = $1")
            .bind(id.to_string())
            .execute(self.conn())
            .await?;
        if done.rows_affected() == 0 {
            return Err(StoreError::not_found("project", id));
        }
        Ok(())
    }
}

/// `InvalidInput` when another project of the workspace has the name (any case).
async fn name_free(tx: &mut Tx, project: &Project) -> Result<()> {
    let taken: Option<String> = sqlx::query_scalar(
        "SELECT id FROM project WHERE workspace_id = $1 AND lower(name) = lower($2) AND id <> $3",
    )
    .bind(project.workspace_id.to_string())
    .bind(&project.name)
    .bind(project.id.to_string())
    .fetch_optional(tx.conn())
    .await?;
    match taken {
        Some(_) => Err(sun_core::Error::InvalidInput(format!(
            "This workspace already has a project named `{}`.",
            project.name
        ))
        .into()),
        None => Ok(()),
    }
}

fn project_from(row: &SqliteRow) -> Result<Project> {
    Ok(Project {
        id: parse(row.try_get("id")?)?,
        workspace_id: parse(row.try_get("workspace_id")?)?,
        name: row.try_get("name")?,
        description: row.try_get("description")?,
        created_at: parse_ts(row.try_get("created_at")?)?,
    })
}
