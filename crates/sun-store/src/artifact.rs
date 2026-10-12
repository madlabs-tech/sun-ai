//! `repo` (a workspace's repositories) and `artifact` (versioned task
//! outputs). Content lives in the blob store; rows hold the hash.

use async_trait::async_trait;
use sqlx::Row;
use sqlx::sqlite::SqliteRow;
use sun_core::model::{Artifact, Repo};
use sun_core::{ArtifactId, BlobHash, RepoId, TaskId, WorkspaceId};

use crate::repo::{json, parse, ts, u32_at};
use crate::{Result, StoreError, Tx};

/// **Repository** for `repo` rows: one workspace's list of repositories,
/// unique by URL within the workspace.
#[async_trait]
pub trait RepoRepo {
    /// Inserts a repo on its workspace; `InvalidInput` when the workspace
    /// already has its URL.
    async fn insert_repo(&mut self, repo: &Repo) -> Result<()>;
    /// The repo, or `NotFound`.
    async fn get_repo(&mut self, id: RepoId) -> Result<Repo>;
    /// The workspace's repos, oldest first.
    async fn repos(&mut self, workspace: WorkspaceId) -> Result<Vec<Repo>>;
    /// The workspace's repo at `url`, if any.
    async fn repo_by_url(&mut self, workspace: WorkspaceId, url: &str) -> Result<Option<Repo>>;
    /// Sets a repo's description and default branch; `NotFound` when missing.
    async fn update_repo(
        &mut self,
        id: RepoId,
        description: &str,
        default_branch: &str,
    ) -> Result<()>;
    /// Deletes the repo; false when there was none.
    async fn delete_repo(&mut self, id: RepoId) -> Result<bool>;
    /// Records the project hook Sun made for the repo (`None`: none).
    async fn set_hook(&mut self, id: RepoId, hook: Option<i64>) -> Result<()>;
}

/// **Repository** for `artifact` rows: an append-only version series per task.
#[async_trait]
pub trait ArtifactRepo {
    /// Stores `blob` as the task's next artifact version, unless it equals the
    /// latest version of the same `name` (then that one is returned).
    async fn add_artifact(
        &mut self,
        task_id: TaskId,
        name: &str,
        blob: &BlobHash,
        mime: &str,
    ) -> Result<Artifact>;
    /// The task's artifacts, oldest version first.
    async fn artifacts_of(&mut self, task_id: TaskId) -> Result<Vec<Artifact>>;
    /// One artifact, or `NotFound`.
    async fn get_artifact(&mut self, id: ArtifactId) -> Result<Artifact>;
}

const REPO_COLUMNS: &str =
    "id, workspace_id, url, default_branch, gate_config_json, description, hook_id";

#[async_trait]
impl RepoRepo for Tx {
    async fn insert_repo(&mut self, repo: &Repo) -> Result<()> {
        if self
            .repo_by_url(repo.workspace_id, &repo.url)
            .await?
            .is_some()
        {
            return Err(
                sun_core::Error::InvalidInput("This repository is already added.".into()).into(),
            );
        }
        sqlx::query(&format!(
            "INSERT INTO repo ({REPO_COLUMNS}, created_at) VALUES ($1, $2, $3, $4, $5, $6, $7, $8)"
        ))
        .bind(repo.id.to_string())
        .bind(repo.workspace_id.to_string())
        .bind(&repo.url)
        .bind(&repo.default_branch)
        .bind(serde_json::to_string(&repo.gate_config)?)
        .bind(&repo.description)
        .bind(repo.hook_id)
        .bind(ts(&chrono::Utc::now()))
        .execute(self.conn())
        .await?;
        Ok(())
    }

    async fn get_repo(&mut self, id: RepoId) -> Result<Repo> {
        let row = sqlx::query(&format!("SELECT {REPO_COLUMNS} FROM repo WHERE id = $1"))
            .bind(id.to_string())
            .fetch_optional(self.conn())
            .await?
            .ok_or_else(|| StoreError::not_found("repo", id))?;
        repo_from(&row)
    }

    async fn repos(&mut self, workspace: WorkspaceId) -> Result<Vec<Repo>> {
        sqlx::query(&format!(
            "SELECT {REPO_COLUMNS} FROM repo WHERE workspace_id = $1 ORDER BY rowid"
        ))
        .bind(workspace.to_string())
        .fetch_all(self.conn())
        .await?
        .iter()
        .map(repo_from)
        .collect()
    }

    async fn repo_by_url(&mut self, workspace: WorkspaceId, url: &str) -> Result<Option<Repo>> {
        sqlx::query(&format!(
            "SELECT {REPO_COLUMNS} FROM repo WHERE workspace_id = $1 AND url = $2"
        ))
        .bind(workspace.to_string())
        .bind(url)
        .fetch_optional(self.conn())
        .await?
        .as_ref()
        .map(repo_from)
        .transpose()
    }

    async fn update_repo(
        &mut self,
        id: RepoId,
        description: &str,
        default_branch: &str,
    ) -> Result<()> {
        let done =
            sqlx::query("UPDATE repo SET description = $1, default_branch = $2 WHERE id = $3")
                .bind(description)
                .bind(default_branch)
                .bind(id.to_string())
                .execute(self.conn())
                .await?;
        if done.rows_affected() == 0 {
            return Err(StoreError::not_found("repo", id));
        }
        Ok(())
    }

    async fn delete_repo(&mut self, id: RepoId) -> Result<bool> {
        let done = sqlx::query("DELETE FROM repo WHERE id = $1")
            .bind(id.to_string())
            .execute(self.conn())
            .await?;
        Ok(done.rows_affected() > 0)
    }

    async fn set_hook(&mut self, id: RepoId, hook: Option<i64>) -> Result<()> {
        let done = sqlx::query("UPDATE repo SET hook_id = $1 WHERE id = $2")
            .bind(hook)
            .bind(id.to_string())
            .execute(self.conn())
            .await?;
        if done.rows_affected() == 0 {
            return Err(StoreError::not_found("repo", id));
        }
        Ok(())
    }
}

#[async_trait]
impl ArtifactRepo for Tx {
    async fn add_artifact(
        &mut self,
        task_id: TaskId,
        name: &str,
        blob: &BlobHash,
        mime: &str,
    ) -> Result<Artifact> {
        let latest = sqlx::query(
            "SELECT * FROM artifact WHERE task_id = $1 AND name = $2 ORDER BY version DESC LIMIT 1",
        )
        .bind(task_id.to_string())
        .bind(name)
        .fetch_optional(self.conn())
        .await?
        .map(|row| artifact_from(&row))
        .transpose()?;
        if let Some(latest) = latest.filter(|a| &a.blob == blob && a.mime == mime) {
            return Ok(latest);
        }
        let version = u32_at(
            &sqlx::query(
                "SELECT COALESCE(MAX(version), 0) + 1 AS version FROM artifact WHERE task_id = $1",
            )
            .bind(task_id.to_string())
            .fetch_one(self.conn())
            .await?,
            "version",
        )?;
        let artifact = Artifact {
            id: ArtifactId::generate(),
            task_id,
            name: name.to_owned(),
            version,
            blob: blob.clone(),
            mime: mime.to_owned(),
        };
        sqlx::query(
            "INSERT INTO artifact (id, task_id, version, blob, mime, name) VALUES ($1, $2, $3, $4, $5, $6)",
        )
        .bind(artifact.id.to_string())
        .bind(task_id.to_string())
        .bind(i64::from(version))
        .bind(blob.as_str())
        .bind(mime)
        .bind(name)
        .execute(self.conn())
        .await?;
        Ok(artifact)
    }

    async fn artifacts_of(&mut self, task_id: TaskId) -> Result<Vec<Artifact>> {
        sqlx::query("SELECT * FROM artifact WHERE task_id = $1 ORDER BY version")
            .bind(task_id.to_string())
            .fetch_all(self.conn())
            .await?
            .iter()
            .map(artifact_from)
            .collect()
    }

    async fn get_artifact(&mut self, id: ArtifactId) -> Result<Artifact> {
        let row = sqlx::query("SELECT * FROM artifact WHERE id = $1")
            .bind(id.to_string())
            .fetch_optional(self.conn())
            .await?
            .ok_or_else(|| StoreError::not_found("artifact", id))?;
        artifact_from(&row)
    }
}

fn repo_from(row: &SqliteRow) -> Result<Repo> {
    Ok(Repo {
        id: parse(row.try_get("id")?)?,
        workspace_id: parse(row.try_get("workspace_id")?)?,
        url: row.try_get("url")?,
        default_branch: row.try_get("default_branch")?,
        gate_config: json(row.try_get("gate_config_json")?)?,
        description: row.try_get("description")?,
        hook_id: row.try_get("hook_id")?,
    })
}

fn artifact_from(row: &SqliteRow) -> Result<Artifact> {
    Ok(Artifact {
        id: parse(row.try_get("id")?)?,
        task_id: parse(row.try_get("task_id")?)?,
        name: row.try_get("name")?,
        version: u32_at(row, "version")?,
        blob: BlobHash::new(row.try_get::<String, _>("blob")?)?,
        mime: row.try_get("mime")?,
    })
}

#[cfg(test)]
mod tests {
    use sun_core::model::{Task, Workflow};
    use sun_core::{TaskStatus, WorkflowId};

    use super::*;
    use crate::{Store, TaskRepo, WorkflowRepo};

    #[tokio::test]
    async fn repos_and_artifact_versions() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).await.unwrap();
        let mut tx = store.begin().await.unwrap();
        let repo = Repo {
            id: RepoId::generate(),
            workspace_id: WorkspaceId::MAIN,
            url: "https://example.com/a.git".into(),
            default_branch: "main".into(),
            gate_config: serde_json::json!({}),
            description: "the web app".into(),
            hook_id: None,
        };
        tx.insert_repo(&repo).await.unwrap();
        assert_eq!(tx.get_repo(repo.id).await.unwrap(), repo);
        assert_eq!(
            tx.repos(WorkspaceId::MAIN).await.unwrap(),
            vec![repo.clone()]
        );

        let workflow = Workflow {
            id: WorkflowId::generate(),
            workspace_id: sun_core::WorkspaceId::MAIN,
            name: "w".into(),
        };
        tx.insert_workflow(&workflow).await.unwrap();
        let task = Task {
            title: "t".into(),
            workflow_id: Some(workflow.id),
            status: TaskStatus::Backlog,
            ..sun_core::model::Task::new(sun_core::WorkspaceId::MAIN, "")
        };
        tx.insert_task(&task).await.unwrap();
        let one = store.blobs().put(b"one".to_vec()).await.unwrap();
        let two = store.blobs().put(b"two".to_vec()).await.unwrap();
        let v1 = tx
            .add_artifact(task.id, "a.md", &one, "text/markdown")
            .await
            .unwrap();
        let same = tx
            .add_artifact(task.id, "a.md", &one, "text/markdown")
            .await
            .unwrap();
        assert_eq!(same, v1, "an unchanged artifact is not a new version");
        let v2 = tx
            .add_artifact(task.id, "a.md", &two, "text/markdown")
            .await
            .unwrap();
        let v3 = tx
            .add_artifact(task.id, "b.md", &one, "text/markdown")
            .await
            .unwrap();
        assert_eq!((v1.version, v2.version, v3.version), (1, 2, 3));
        assert_eq!(
            tx.artifacts_of(task.id).await.unwrap(),
            vec![v1, v2.clone(), v3]
        );
        assert_eq!(tx.get_artifact(v2.id).await.unwrap(), v2);
    }
}
