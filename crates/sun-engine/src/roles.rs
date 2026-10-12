//! Workspace roles, part of the **Facade** for the API: what a member may
//! do in each workspace ([`Access`]), the deployment owner flag, and
//! membership changes (a member's role, removing a member, leaving). A
//! workspace always keeps an owner: the store refuses the change that would
//! leave it none. Who may make a change is the API's check
//! ([`sun_core::model::Role::covers`]). Each change commits with its audit row.

use sun_core::model::{
    AuditAction, Member, Permission, Role, WorkspaceMember, permissions_of, roles_of,
};
use sun_core::{MemberId, Result, WorkspaceId};
use sun_store::{AuditRepo, MemberRepo, MembershipRepo, Tx, WorkspaceRepo};

use crate::{Engine, Principal};

/// What a member may do: who they are, their role in each workspace, and
/// the permissions and approval roles those give.
#[derive(Debug, Clone)]
pub struct Access {
    /// The member row.
    pub member: Member,
    /// Their workspaces and role in each, by workspace id.
    pub memberships: Vec<WorkspaceMember>,
    /// The union of their roles' permissions (all of them for a
    /// deployment owner).
    // ponytail: one set for every workspace; per-workspace checks
    // ([`Access::can_in`]) replace it where a route names its workspace.
    pub permissions: Vec<Permission>,
    /// The workspaces they see: their memberships, or `None` for every
    /// workspace (deployment owners).
    pub workspaces: Option<Vec<WorkspaceId>>,
}

impl Access {
    /// True when one of their roles grants `permission`.
    pub fn can(&self, permission: Permission) -> bool {
        self.permissions.contains(&permission)
    }

    /// True when they see `workspace`.
    pub fn sees(&self, workspace: WorkspaceId) -> bool {
        self.workspaces
            .as_ref()
            .is_none_or(|b| b.contains(&workspace))
    }

    /// Their role in `workspace`, if they are a member.
    pub fn role_in(&self, workspace: WorkspaceId) -> Option<Role> {
        self.memberships
            .iter()
            .find(|m| m.workspace_id == workspace)
            .map(|m| m.role)
    }

    /// True when they may `permission` in `workspace`: a deployment owner
    /// always, else their role there grants it.
    pub fn can_in(&self, workspace: WorkspaceId, permission: Permission) -> bool {
        self.member.is_owner
            || self
                .role_in(workspace)
                .is_some_and(|r| r.permissions().contains(&permission))
    }

    /// Who they are for approval `who:`: their username and the keys of
    /// all their roles (and those below them).
    pub fn principal(&self) -> Principal {
        let roles: Vec<Role> = self.memberships.iter().map(|m| m.role).collect();
        Principal {
            user: self.member.username.clone(),
            roles: roles_of(self.member.is_owner, &roles),
        }
    }

    /// Who they are for an approval on a task in `workspace`: their
    /// username and the keys of their role there (and those below it).
    pub fn principal_in(&self, workspace: WorkspaceId) -> Principal {
        let roles: Vec<Role> = self.role_in(workspace).into_iter().collect();
        Principal {
            user: self.member.username.clone(),
            roles: roles_of(self.member.is_owner, &roles),
        }
    }
}

async fn access(tx: &mut Tx, member: Member) -> Result<Access> {
    let memberships = tx.memberships_of(member.id).await?;
    let roles: Vec<Role> = memberships.iter().map(|m| m.role).collect();
    let permissions = permissions_of(member.is_owner, &roles);
    let workspaces =
        (!member.is_owner).then(|| memberships.iter().map(|m| m.workspace_id).collect());
    Ok(Access {
        permissions,
        member,
        memberships,
        workspaces,
    })
}

/// `alice in Ops` for an audit target.
async fn target(tx: &mut Tx, workspace: WorkspaceId, member: MemberId) -> Result<String> {
    let name = tx.get_workspace(workspace).await?.name;
    let username = tx.get_member(member).await?.username;
    Ok(format!("{username} in {name}"))
}

impl Engine {
    /// What member `id` may do, read in one snapshot; `NotFound` when gone.
    pub async fn member_access(&self, id: MemberId) -> Result<Access> {
        let mut tx = self.store.read().await?;
        let member = tx.get_member(id).await?;
        access(&mut tx, member).await
    }

    /// What `member` may do (their memberships read now).
    pub(crate) async fn access_of(&self, member: Member) -> Result<Access> {
        access(&mut self.store.read().await?, member).await
    }

    /// The members of `workspace` with their roles, oldest member first.
    pub async fn workspace_members(&self, workspace: WorkspaceId) -> Result<Vec<WorkspaceMember>> {
        let mut tx = self.store.read().await?;
        tx.get_workspace(workspace).await?;
        Ok(tx.members_of(workspace).await?)
    }

    /// Changes member `id`'s role in `workspace`, audited. `NotFound` when
    /// they aren't a member; `InvalidInput` when it would leave the
    /// workspace no owner.
    pub async fn set_role(
        &self,
        workspace: WorkspaceId,
        id: MemberId,
        role: Role,
        actor: &str,
    ) -> Result<WorkspaceMember> {
        let mut tx = self.store.begin().await?;
        let before = tx.role_of(workspace, id).await?;
        tx.set_role(workspace, id, role).await?;
        let detail = format!("{} -> {role}", before.map_or("none", Role::as_str));
        let target = target(&mut tx, workspace, id).await?;
        tx.append_audit(actor, AuditAction::RoleChange, &target, &detail)
            .await?;
        let member = tx
            .members_of(workspace)
            .await?
            .into_iter()
            .find(|m| m.member_id == id);
        tx.commit().await?;
        member.ok_or_else(|| sun_core::Error::NotFound(format!("workspace member {id}")))
    }

    /// Takes member `id` out of `workspace` (themselves: they leave),
    /// audited. `NotFound` when they aren't a member; `InvalidInput` for
    /// its last owner.
    pub async fn remove_member(
        &self,
        workspace: WorkspaceId,
        id: MemberId,
        actor: &str,
    ) -> Result<()> {
        let mut tx = self.store.begin().await?;
        let target = target(&mut tx, workspace, id).await?;
        if !tx.remove_member(workspace, id).await? {
            return Err(sun_core::Error::NotFound(format!("workspace member {id}")));
        }
        tx.append_audit(actor, AuditAction::RoleChange, &target, "removed")
            .await?;
        tx.commit().await?;
        Ok(())
    }

    /// Member `id` leaves `workspace`, audited as them; as
    /// [`Engine::remove_member`].
    pub async fn leave(&self, workspace: WorkspaceId, id: MemberId) -> Result<()> {
        let username = self.store.read().await?.get_member(id).await?.username;
        self.remove_member(workspace, id, &format!("user:{username}"))
            .await
    }

    /// Makes member `id` a deployment owner, or not.
    pub async fn set_member_owner(
        &self,
        id: MemberId,
        is_owner: bool,
        actor: &str,
    ) -> Result<Member> {
        let mut tx = self.store.begin().await?;
        tx.set_member_owner(id, is_owner).await?;
        let member = tx.get_member(id).await?;
        let detail = match is_owner {
            true => "made deployment owner",
            false => "no longer deployment owner",
        };
        tx.append_audit(actor, AuditAction::RoleChange, &member.username, detail)
            .await?;
        tx.commit().await?;
        Ok(member)
    }
}
