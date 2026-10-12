//! Groups, part of the **Facade** for the API: roles an admin can
//! create, rename, delete and customize, who is in them, and the owner flag.
//! A member's permissions are the union of their groups' ([`permissions_of`]);
//! their approval roles are the group keys ([`roles_of`]). Each change
//! commits with its audit row.

use sun_core::model::{AuditAction, Group, Member, Permission, permissions_of, roles_of};
use sun_core::{GroupId, MemberId, Result, WorkspaceId};
use sun_store::{AuditRepo, GroupRepo, MemberRepo, Tx, WorkspaceRepo};

use crate::{Engine, Principal};

/// What a member may do: who they are, their groups, and the permissions
/// and approval roles those give.
#[derive(Debug, Clone)]
pub struct Access {
    /// The member row.
    pub member: Member,
    /// Their groups.
    pub groups: Vec<Group>,
    /// The union of the groups' permissions (all of them for an owner).
    pub permissions: Vec<Permission>,
    /// The workspaces they see: those attached to their groups, or `None` for
    /// every workspace (owners and `access_all_workspaces`).
    pub workspaces: Option<Vec<WorkspaceId>>,
}

impl Access {
    /// True when they may `permission`.
    pub fn can(&self, permission: Permission) -> bool {
        self.permissions.contains(&permission)
    }

    /// True when they see `workspace`.
    pub fn sees(&self, workspace: WorkspaceId) -> bool {
        self.workspaces
            .as_ref()
            .is_none_or(|b| b.contains(&workspace))
    }

    /// Who they are for approval `who:`: their username and group keys
    /// (plus `owner`).
    pub fn principal(&self) -> Principal {
        Principal {
            user: self.member.username.clone(),
            roles: roles_of(self.member.is_owner, &self.groups),
        }
    }
}

async fn access(tx: &mut Tx, member: Member) -> Result<Access> {
    let groups = tx.groups_of_member(member.id).await?;
    let permissions = permissions_of(member.is_owner, &groups);
    let workspaces = match permissions.contains(&Permission::AccessAllWorkspaces) {
        true => None,
        false => Some(tx.workspaces_of_member(member.id).await?),
    };
    Ok(Access {
        permissions,
        member,
        groups,
        workspaces,
    })
}

/// `names` of `groups` for an audit detail, e.g. `Admin, Builders`.
fn names(groups: &[Group]) -> String {
    let names: Vec<&str> = groups.iter().map(|g| g.name.as_str()).collect();
    match names.is_empty() {
        true => "no groups".into(),
        false => names.join(", "),
    }
}

impl Engine {
    /// What member `id` may do, read in one snapshot; `NotFound` when gone.
    pub async fn member_access(&self, id: MemberId) -> Result<Access> {
        let mut tx = self.store.read().await?;
        let member = tx.get_member(id).await?;
        access(&mut tx, member).await
    }

    /// What `member` may do (their groups read now).
    pub(crate) async fn access_of(&self, member: Member) -> Result<Access> {
        access(&mut self.store.read().await?, member).await
    }

    /// Every group, oldest first.
    pub async fn groups(&self) -> Result<Vec<Group>> {
        Ok(self.store.read().await?.groups().await?)
    }

    /// Creates a group named `name` with `permissions`; its key comes from
    /// the name and never changes.
    pub async fn create_group(
        &self,
        name: &str,
        permissions: &[Permission],
        actor: &str,
    ) -> Result<Group> {
        let name = Group::clean_name(name)?;
        let mut tx = self.store.begin().await?;
        let group = tx.insert_group(&name, permissions).await?;
        let granted: Vec<&str> = group.permissions.iter().map(|p| p.as_str()).collect();
        let detail = format!(
            "created as role:{}; permissions: {}",
            group.key,
            granted.join(", ")
        );
        tx.append_audit(actor, AuditAction::GroupChange, &group.name, &detail)
            .await?;
        tx.commit().await?;
        Ok(group)
    }

    /// Renames a group and/or replaces its permissions (`None` keeps it).
    pub async fn update_group(
        &self,
        id: GroupId,
        name: Option<&str>,
        permissions: Option<&[Permission]>,
        actor: &str,
    ) -> Result<Group> {
        let name = name.map(Group::clean_name).transpose()?;
        let mut tx = self.store.begin().await?;
        let before = tx.get_group(id).await?;
        if let Some(name) = &name {
            tx.rename_group(id, name).await?;
            let detail = format!("renamed from {}", before.name);
            tx.append_audit(actor, AuditAction::GroupChange, name, &detail)
                .await?;
        }
        if let Some(permissions) = permissions {
            tx.set_group_permissions(id, permissions).await?;
            let after = tx.get_group(id).await?;
            let granted: Vec<&str> = after.permissions.iter().map(|p| p.as_str()).collect();
            let detail = format!("permissions: {}", granted.join(", "));
            tx.append_audit(actor, AuditAction::GroupChange, &after.name, &detail)
                .await?;
        }
        let group = tx.get_group(id).await?;
        tx.commit().await?;
        Ok(group)
    }

    /// Deletes a group; its members leave it.
    pub async fn delete_group(&self, id: GroupId, actor: &str) -> Result<()> {
        let mut tx = self.store.begin().await?;
        let group = tx.get_group(id).await?;
        tx.delete_group(id).await?;
        let detail = format!("deleted (was role:{})", group.key);
        tx.append_audit(actor, AuditAction::GroupChange, &group.name, &detail)
            .await?;
        tx.commit().await?;
        Ok(())
    }

    /// Puts member `id` in exactly `groups`.
    pub async fn set_member_groups(
        &self,
        id: MemberId,
        groups: &[GroupId],
        actor: &str,
    ) -> Result<Member> {
        let mut tx = self.store.begin().await?;
        let before = tx.groups_of_member(id).await?;
        tx.set_member_groups(id, groups).await?;
        let after = tx.groups_of_member(id).await?;
        let member = tx.get_member(id).await?;
        let detail = format!("{} -> {}", names(&before), names(&after));
        tx.append_audit(actor, AuditAction::RoleChange, &member.username, &detail)
            .await?;
        tx.commit().await?;
        Ok(member)
    }

    /// Makes `members` exactly the members of group `id`.
    pub async fn set_group_members(
        &self,
        id: GroupId,
        members: &[MemberId],
        actor: &str,
    ) -> Result<Vec<Member>> {
        let mut tx = self.store.begin().await?;
        tx.set_group_members(id, members).await?;
        let group = tx.get_group(id).await?;
        let all = tx.members().await?;
        let usernames: Vec<&str> = all
            .iter()
            .filter(|m| m.groups.contains(&id))
            .map(|m| m.username.as_str())
            .collect();
        let detail = format!("members: {}", usernames.join(", "));
        tx.append_audit(actor, AuditAction::GroupChange, &group.name, &detail)
            .await?;
        tx.commit().await?;
        Ok(all.into_iter().filter(|m| m.groups.contains(&id)).collect())
    }

    /// Makes member `id` an owner, or not.
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
            true => "made owner",
            false => "no longer owner",
        };
        tx.append_audit(actor, AuditAction::RoleChange, &member.username, detail)
            .await?;
        tx.commit().await?;
        Ok(member)
    }
}
