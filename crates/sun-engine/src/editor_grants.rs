//! Editor grants, part of the **Facade** for the API: list, grant and
//! revoke a workspace's editor capabilities, read the ones that apply to a
//! repo, and the audit rows of what the editor does. Who may grant is the
//! API's check; what a grant gives is [`sun_core::editor_grant::capabilities_of`].
//! Each change commits with its audit row; file contents and keystrokes are
//! never audited.

use sun_core::editor_grant::{EditorCapability, EditorGrant, GrantSubject, check_viewer_cap};
use sun_core::model::{AuditAction, Repo, Role};
use sun_core::{EditorGrantId, Error, HostId, MemberId, RepoId, Result, WorkspaceId};
use sun_store::{AuditRepo, EditorGrantRepo, GroupRepo, MemberRepo, MembershipRepo, RepoRepo, Tx};

use crate::Engine;

/// `ana`, `role:member`, `group:Team` or `all` for an audit target.
async fn subject_name(tx: &mut Tx, subject: GrantSubject) -> Result<String> {
    Ok(match subject {
        GrantSubject::All => "all".into(),
        GrantSubject::Role(role) => format!("role:{role}"),
        GrantSubject::Group(id) => format!("group:{}", tx.get_group(id).await?.name),
        GrantSubject::Member(id) => tx.get_member(id).await?.username,
    })
}

/// `editor.write on <repo url>` or `editor.write on every repo`.
async fn grant_detail(tx: &mut Tx, grant: &EditorGrant, verb: &str) -> Result<String> {
    let on = match grant.repo_id {
        Some(repo) => tx.get_repo(repo).await?.url,
        None => "every repo".into(),
    };
    Ok(format!("{verb} {} on {on}", grant.capability))
}

/// Repo `repo` of `workspace`; `NotFound` when it is another workspace's.
async fn repo_in(tx: &mut Tx, workspace: WorkspaceId, repo: RepoId) -> Result<Repo> {
    match tx.get_repo(repo).await {
        Ok(found) if found.workspace_id == workspace => Ok(found),
        Ok(_) | Err(sun_store::StoreError::Core(Error::NotFound(_))) => {
            Err(Error::NotFound(format!("repo {repo}")))
        }
        Err(e) => Err(e.into()),
    }
}

impl Engine {
    /// Every editor grant of `workspace`, oldest first.
    pub async fn editor_grants(&self, workspace: WorkspaceId) -> Result<Vec<EditorGrant>> {
        Ok(self.store.read().await?.editor_grants(workspace).await?)
    }

    /// The grants of `workspace` that apply to `repo`, read now in one
    /// query (never cached, so a revoke applies to the next request);
    /// `NotFound` when `repo` isn't a repo of `workspace`.
    pub async fn editor_grants_on(
        &self,
        workspace: WorkspaceId,
        repo: RepoId,
    ) -> Result<Vec<EditorGrant>> {
        let mut tx = self.store.read().await?;
        tx.editor_grants_on(workspace, repo)
            .await?
            .ok_or_else(|| Error::NotFound(format!("repo {repo}")))
    }

    /// Grants `capability` to `subject` in `workspace` (on `repo`, or every
    /// repo there), audited as `actor`. `NotFound` for a repo of another
    /// workspace; `InvalidInput` for an unknown group, a member not in the
    /// workspace, more than `editor.read` for a viewer (the role or a
    /// member who is one), or a grant that already exists.
    pub async fn grant_editor(
        &self,
        workspace: WorkspaceId,
        repo: Option<RepoId>,
        subject: GrantSubject,
        capability: EditorCapability,
        granted_by: Option<MemberId>,
        actor: &str,
    ) -> Result<EditorGrant> {
        let mut tx = self.store.begin().await?;
        if let Some(repo) = repo {
            repo_in(&mut tx, workspace, repo).await?;
        }
        match subject {
            GrantSubject::All => {}
            GrantSubject::Role(role) => check_viewer_cap(role, capability)?,
            // A viewer in the group holds `editor.read` at most (capped at
            // resolution).
            GrantSubject::Group(id) => match tx.get_group(id).await {
                Ok(_) => {}
                Err(sun_store::StoreError::Core(Error::NotFound(_))) => {
                    return Err(Error::InvalidInput("There is no such group.".into()));
                }
                Err(e) => return Err(e.into()),
            },
            GrantSubject::Member(id) => {
                let role: Role = tx.role_of(workspace, id).await?.ok_or_else(|| {
                    Error::InvalidInput("They aren't a member of this workspace.".into())
                })?;
                check_viewer_cap(role, capability)?;
            }
        }
        let grant = EditorGrant {
            id: EditorGrantId::generate(),
            workspace_id: workspace,
            repo_id: repo,
            subject,
            capability,
            granted_by,
            created_at: chrono::Utc::now(),
        };
        tx.insert_editor_grant(&grant).await?;
        let target = subject_name(&mut tx, subject).await?;
        let detail = grant_detail(&mut tx, &grant, "granted").await?;
        tx.append_audit(actor, AuditAction::EditorGrant, &target, &detail)
            .await?;
        tx.commit().await?;
        Ok(grant)
    }

    /// Revokes grant `id` of `workspace`, audited as `actor`; `NotFound`
    /// when the workspace has no such grant.
    pub async fn revoke_editor(
        &self,
        workspace: WorkspaceId,
        id: EditorGrantId,
        actor: &str,
    ) -> Result<EditorGrant> {
        let mut tx = self.store.begin().await?;
        let grant = tx.delete_editor_grant(workspace, id).await?;
        let target = subject_name(&mut tx, grant.subject).await?;
        let detail = grant_detail(&mut tx, &grant, "revoked").await?;
        tx.append_audit(actor, AuditAction::EditorGrant, &target, &detail)
            .await?;
        tx.commit().await?;
        Ok(grant)
    }

    /// Appends the audit row of what `actor` did in the editor on `repo`
    /// of `workspace` through `host` (`action` is
    /// [`AuditAction::EditorFile`] or [`AuditAction::Terminal`]): the repo,
    /// host and `detail` (the action and path), never contents.
    pub async fn audit_editor(
        &self,
        actor: &str,
        workspace: WorkspaceId,
        repo: RepoId,
        host: HostId,
        action: AuditAction,
        detail: &str,
    ) -> Result<()> {
        let mut tx = self.store.begin().await?;
        let url = repo_in(&mut tx, workspace, repo).await?.url;
        let target = format!("{url} on host {host}");
        tx.append_audit(actor, action, &target, detail).await?;
        tx.commit().await?;
        Ok(())
    }
}
