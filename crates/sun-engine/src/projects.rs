//! Projects, part of the **Facade** for the API: named groups of one
//! workspace's tasks. Each change commits with its audit row (on the
//! workspace).

use sun_core::model::{AuditAction, Project};
use sun_core::{ProjectId, Result, WorkspaceId};
use sun_store::{AuditRepo, ProjectRepo, Tx, WorkspaceRepo};

use crate::Engine;

/// `PATCH /projects/{id}`: the fields to change; absent ones stay.
#[derive(Debug, Clone, Default, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProjectChange {
    /// New name (1–64 characters, unique in the workspace ignoring case).
    pub name: Option<String>,
    /// New description.
    pub description: Option<String>,
}

impl Engine {
    /// A workspace's projects, by name; `NotFound` for a missing workspace.
    pub async fn projects_of_workspace(&self, workspace: WorkspaceId) -> Result<Vec<Project>> {
        let mut tx = self.store.read().await?;
        tx.get_workspace(workspace).await?;
        Ok(tx.projects_of_workspace(workspace).await?)
    }

    /// Creates a project on `workspace`.
    pub async fn create_project(
        &self,
        workspace: WorkspaceId,
        name: &str,
        description: &str,
        actor: &str,
    ) -> Result<Project> {
        let project = Project {
            id: ProjectId::generate(),
            workspace_id: workspace,
            name: Project::clean_name(name)?,
            description: Project::clean_description(description)?,
            created_at: chrono::Utc::now(),
        };
        let mut tx = self.store.begin().await?;
        tx.get_workspace(workspace).await?;
        tx.insert_project(&project).await?;
        audit(&mut tx, &project, actor, "created").await?;
        tx.commit().await?;
        Ok(project)
    }

    /// Changes the fields `change` names.
    pub async fn update_project(
        &self,
        id: ProjectId,
        change: &ProjectChange,
        actor: &str,
    ) -> Result<Project> {
        let mut tx = self.store.begin().await?;
        let mut project = tx.get_project(id).await?;
        let before = project.name.clone();
        if let Some(name) = &change.name {
            project.name = Project::clean_name(name)?;
        }
        if let Some(description) = &change.description {
            project.description = Project::clean_description(description)?;
        }
        tx.update_project(&project).await?;
        audit(&mut tx, &project, actor, &format!("updated (was {before})")).await?;
        tx.commit().await?;
        Ok(project)
    }

    /// Deletes a project.
    pub async fn delete_project(&self, id: ProjectId, actor: &str) -> Result<()> {
        let mut tx = self.store.begin().await?;
        let project = tx.get_project(id).await?;
        // T23: refuse while tasks reference the project (`task.project_id`).
        tx.delete_project(id).await?;
        audit(&mut tx, &project, actor, "deleted").await?;
        Ok(tx.commit().await?)
    }
}

/// A `workspace_change` row on the project's workspace: `project <name> <what>`.
async fn audit(tx: &mut Tx, project: &Project, actor: &str, what: &str) -> Result<()> {
    let workspace = tx.get_workspace(project.workspace_id).await?.name;
    let detail = format!("project {} {what}", project.name);
    tx.append_audit(actor, AuditAction::WorkspaceChange, &workspace, &detail)
        .await?;
    Ok(())
}
