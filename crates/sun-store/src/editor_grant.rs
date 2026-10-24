//! `editor_grant`: the editor capabilities granted in each workspace
//! ([`sun_core::editor_grant`]). Owners' implicit capabilities are never
//! stored. A grant exists once per workspace, repo, subject and capability;
//! a member's own grants go when they leave the workspace (a trigger).

use async_trait::async_trait;
use sqlx::Row;
use sqlx::sqlite::SqliteRow;
use sun_core::editor_grant::{EditorGrant, GrantSubject};
use sun_core::{EditorGrantId, RepoId, WorkspaceId};

use crate::repo::{parse, parse_ts, ts};
use crate::{Result, StoreError, Tx};

/// **Repository** for editor grants. Write the audit row in the same `Tx`.
#[async_trait]
pub trait EditorGrantRepo {
    /// Every grant of `workspace`, oldest first: one query, read per request
    /// so a revoke applies to the next one.
    async fn editor_grants(&mut self, workspace: WorkspaceId) -> Result<Vec<EditorGrant>>;
    /// The grants of `workspace` that apply to `repo` (its own and the
    /// workspace-wide ones), in one query; `None` when `repo` isn't a repo
    /// of `workspace`.
    async fn editor_grants_on(
        &mut self,
        workspace: WorkspaceId,
        repo: RepoId,
    ) -> Result<Option<Vec<EditorGrant>>>;
    /// Stores `grant`. `InvalidInput` when the same grant exists.
    async fn insert_editor_grant(&mut self, grant: &EditorGrant) -> Result<()>;
    /// Deletes grant `id` of `workspace` and returns it; `NotFound` when
    /// `workspace` has no such grant.
    async fn delete_editor_grant(
        &mut self,
        workspace: WorkspaceId,
        id: EditorGrantId,
    ) -> Result<EditorGrant>;
}

const COLUMNS: &str =
    "id, workspace_id, repo_id, subject_type, subject_id, capability, granted_by, created_at";

fn grant_from(row: &SqliteRow) -> Result<EditorGrant> {
    let repo: Option<String> = row.try_get("repo_id")?;
    let granted_by: Option<String> = row.try_get("granted_by")?;
    let subject_type: String = row.try_get("subject_type")?;
    let subject_id: Option<String> = row.try_get("subject_id")?;
    Ok(EditorGrant {
        id: parse(row.try_get("id")?)?,
        workspace_id: parse(row.try_get("workspace_id")?)?,
        repo_id: repo.map(parse).transpose()?,
        subject: GrantSubject::from_columns(&subject_type, subject_id.as_deref())?,
        capability: parse(row.try_get("capability")?)?,
        granted_by: granted_by.map(parse).transpose()?,
        created_at: parse_ts(row.try_get("created_at")?)?,
    })
}

#[async_trait]
impl EditorGrantRepo for Tx {
    async fn editor_grants(&mut self, workspace: WorkspaceId) -> Result<Vec<EditorGrant>> {
        sqlx::query(&format!(
            "SELECT {COLUMNS} FROM editor_grant WHERE workspace_id = $1 ORDER BY created_at, id"
        ))
        .bind(workspace.to_string())
        .fetch_all(self.conn())
        .await?
        .iter()
        .map(grant_from)
        .collect()
    }

    async fn editor_grants_on(
        &mut self,
        workspace: WorkspaceId,
        repo: RepoId,
    ) -> Result<Option<Vec<EditorGrant>>> {
        let columns = COLUMNS
            .split(", ")
            .map(|c| format!("g.{c}"))
            .collect::<Vec<_>>()
            .join(", ");
        let rows = sqlx::query(&format!(
            "SELECT {columns} FROM repo r
             LEFT JOIN editor_grant g
               ON g.workspace_id = r.workspace_id AND (g.repo_id IS NULL OR g.repo_id = r.id)
             WHERE r.id = $2 AND r.workspace_id = $1
             ORDER BY g.created_at, g.id"
        ))
        .bind(workspace.to_string())
        .bind(repo.to_string())
        .fetch_all(self.conn())
        .await?;
        if rows.is_empty() {
            return Ok(None);
        }
        let mut grants = Vec::new();
        for row in &rows {
            if row.try_get::<Option<String>, _>("id")?.is_some() {
                grants.push(grant_from(row)?);
            }
        }
        Ok(Some(grants))
    }

    async fn insert_editor_grant(&mut self, grant: &EditorGrant) -> Result<()> {
        let (subject_type, subject_id) = grant.subject.to_columns();
        let done = sqlx::query(&format!(
            "INSERT INTO editor_grant ({COLUMNS}) VALUES ($1, $2, $3, $4, $5, $6, $7, $8)"
        ))
        .bind(grant.id.to_string())
        .bind(grant.workspace_id.to_string())
        .bind(grant.repo_id.map(|r| r.to_string()))
        .bind(subject_type)
        .bind(subject_id)
        .bind(grant.capability.as_str())
        .bind(grant.granted_by.map(|m| m.to_string()))
        .bind(ts(&grant.created_at))
        .execute(self.conn())
        .await;
        match done {
            Ok(_) => Ok(()),
            Err(sqlx::Error::Database(db)) if db.is_unique_violation() => {
                Err(sun_core::Error::InvalidInput("That grant already exists.".into()).into())
            }
            Err(e) => Err(e.into()),
        }
    }

    async fn delete_editor_grant(
        &mut self,
        workspace: WorkspaceId,
        id: EditorGrantId,
    ) -> Result<EditorGrant> {
        let row = sqlx::query(&format!(
            "DELETE FROM editor_grant WHERE workspace_id = $1 AND id = $2 RETURNING {COLUMNS}"
        ))
        .bind(workspace.to_string())
        .bind(id.to_string())
        .fetch_optional(self.conn())
        .await?;
        match row {
            Some(row) => grant_from(&row),
            None => Err(StoreError::not_found("editor grant", id)),
        }
    }
}

#[cfg(test)]
mod tests {
    use sun_core::editor_grant::EditorCapability;
    use sun_core::model::Role;

    use super::*;
    use crate::{MemberRepo, MembershipRepo, Store};

    fn grant(subject: GrantSubject, capability: EditorCapability) -> EditorGrant {
        EditorGrant {
            id: EditorGrantId::generate(),
            workspace_id: WorkspaceId::MAIN,
            repo_id: None,
            subject,
            capability,
            granted_by: None,
            created_at: chrono::Utc::now(),
        }
    }

    #[tokio::test]
    async fn grants_round_trip_once_each_and_go_with_their_member() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).await.unwrap();
        let mut tx = store.begin().await.unwrap();
        let ana = tx
            .upsert_member("https://idp", "ana", "ana", Some("ana@corp.io"), false)
            .await
            .unwrap();
        tx.add_member(WorkspaceId::MAIN, ana.id, Role::Member)
            .await
            .unwrap();
        let to_all = grant(GrantSubject::All, EditorCapability::Read);
        let to_ana = grant(GrantSubject::Member(ana.id), EditorCapability::Terminal);
        tx.insert_editor_grant(&to_all).await.unwrap();
        tx.insert_editor_grant(&to_ana).await.unwrap();
        let again = grant(GrantSubject::All, EditorCapability::Read);
        assert!(tx.insert_editor_grant(&again).await.is_err(), "once");
        let stored = tx.editor_grants(WorkspaceId::MAIN).await.unwrap();
        assert_eq!(stored.len(), 2);
        assert!(stored.iter().all(|g| g.repo_id.is_none()));
        assert!(stored.contains(&to_all) && stored.contains(&to_ana));

        let repo = sun_core::model::Repo {
            id: RepoId::generate(),
            workspace_id: WorkspaceId::MAIN,
            url: "https://git.test/a.git".into(),
            default_branch: "main".into(),
            gate_config: serde_json::json!({}),
            description: String::new(),
            hook_id: None,
        };
        crate::RepoRepo::insert_repo(&mut tx, &repo).await.unwrap();
        let elsewhere = EditorGrant {
            repo_id: Some(RepoId::generate()),
            ..grant(GrantSubject::All, EditorCapability::Write)
        };
        assert!(
            tx.insert_editor_grant(&elsewhere).await.is_err(),
            "unknown repo"
        );
        let scoped = EditorGrant {
            repo_id: Some(repo.id),
            ..grant(GrantSubject::All, EditorCapability::Write)
        };
        tx.insert_editor_grant(&scoped).await.unwrap();
        let on = tx
            .editor_grants_on(WorkspaceId::MAIN, repo.id)
            .await
            .unwrap();
        assert_eq!(on.unwrap().len(), 3);
        let other = WorkspaceId::generate();
        assert_eq!(tx.editor_grants_on(other, repo.id).await.unwrap(), None);
        let missing = tx.editor_grants_on(WorkspaceId::MAIN, RepoId::generate());
        assert_eq!(missing.await.unwrap(), None);
        tx.delete_editor_grant(WorkspaceId::MAIN, scoped.id)
            .await
            .unwrap();

        assert!(tx.delete_editor_grant(other, to_all.id).await.is_err());
        let gone = tx
            .delete_editor_grant(WorkspaceId::MAIN, to_all.id)
            .await
            .unwrap();
        assert_eq!(gone, to_all);
        assert!(
            tx.delete_editor_grant(WorkspaceId::MAIN, to_all.id)
                .await
                .is_err()
        );

        tx.remove_member(WorkspaceId::MAIN, ana.id).await.unwrap();
        assert!(
            tx.editor_grants(WorkspaceId::MAIN)
                .await
                .unwrap()
                .is_empty()
        );
    }
}
