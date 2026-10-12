//! `workspace_member` and `workspace_invitation`: who is in which workspace
//! with which role, and the emailed invitations into a workspace. A
//! workspace always keeps at least one owner: the store refuses the change
//! that would leave it none. Invitation tokens are stored as
//! `sha256(token)` only (the caller hashes).

use async_trait::async_trait;
use sqlx::Row;
use sqlx::sqlite::SqliteRow;
use sun_core::model::{Role, WorkspaceInvitation, WorkspaceMember};
use sun_core::{InvitationId, MemberId, Timestamp, WorkspaceId};

use crate::member::second;
use crate::repo::{parse, parse_ts, ts};
use crate::{Result, StoreError, Tx};

/// **Repository** for workspace memberships.
#[async_trait]
pub trait MembershipRepo {
    /// The workspaces `member` is in, with their role, by workspace id.
    async fn memberships_of(&mut self, member: MemberId) -> Result<Vec<WorkspaceMember>>;
    /// The members of `workspace`, with their role, oldest member first.
    async fn members_of(&mut self, workspace: WorkspaceId) -> Result<Vec<WorkspaceMember>>;
    /// `member`'s role in `workspace`, if they are in it.
    async fn role_of(&mut self, workspace: WorkspaceId, member: MemberId) -> Result<Option<Role>>;
    /// Adds `member` to `workspace` as `role` (joined now). `InvalidInput`
    /// when they already are a member; `NotFound` for an unknown workspace
    /// or member.
    async fn add_member(
        &mut self,
        workspace: WorkspaceId,
        member: MemberId,
        role: Role,
    ) -> Result<WorkspaceMember>;
    /// Changes `member`'s role in `workspace`. `NotFound` when they aren't
    /// in it; `InvalidInput` when it would leave the workspace no owner.
    async fn set_role(
        &mut self,
        workspace: WorkspaceId,
        member: MemberId,
        role: Role,
    ) -> Result<()>;
    /// Takes `member` out of `workspace`; `false` when they weren't in it.
    /// `InvalidInput` when they are its last owner.
    async fn remove_member(&mut self, workspace: WorkspaceId, member: MemberId) -> Result<bool>;
    /// How many owners `workspace` has.
    async fn owner_count(&mut self, workspace: WorkspaceId) -> Result<i64>;
}

/// **Repository** for workspace invitations.
#[async_trait]
pub trait InvitationRepo {
    /// Stores `invitation` under `token_hash`, replacing a pending one for
    /// the same workspace and email (inviting again re-sends).
    async fn insert_invitation(
        &mut self,
        invitation: &WorkspaceInvitation,
        token_hash: &str,
    ) -> Result<()>;
    /// The invitation whose token hashes to `token_hash`, in any state.
    async fn invitation_by_token_hash(
        &mut self,
        token_hash: &str,
    ) -> Result<Option<WorkspaceInvitation>>;
    /// One invitation, or `NotFound`.
    async fn get_invitation(&mut self, id: InvitationId) -> Result<WorkspaceInvitation>;
    /// `workspace`'s pending invitations (not accepted, declined or
    /// expired), newest first.
    async fn invitations(&mut self, workspace: WorkspaceId) -> Result<Vec<WorkspaceInvitation>>;
    /// The pending invitations to `email` (any case), newest first.
    async fn invitations_by_email(&mut self, email: &str) -> Result<Vec<WorkspaceInvitation>>;
    /// Marks a pending invitation accepted (now); `false` when it isn't
    /// pending any more (accepted, declined, expired or unknown).
    async fn accept_invitation(&mut self, id: InvitationId) -> Result<bool>;
    /// Marks a pending invitation declined (now); `false` as for accept.
    async fn decline_invitation(&mut self, id: InvitationId) -> Result<bool>;
    /// Ends a pending invitation now (revoked); `false` as for accept.
    async fn expire_invitation(&mut self, id: InvitationId) -> Result<bool>;
}

const LAST_OWNER: &str = "A workspace keeps at least one owner: make someone else owner first.";

/// `InvalidInput` when `member` is `workspace`'s only owner.
async fn not_last_owner(tx: &mut Tx, workspace: WorkspaceId, member: MemberId) -> Result<()> {
    if tx.role_of(workspace, member).await? == Some(Role::Owner)
        && tx.owner_count(workspace).await? <= 1
    {
        return Err(sun_core::Error::InvalidInput(LAST_OWNER.into()).into());
    }
    Ok(())
}

fn membership_from(row: &SqliteRow) -> Result<WorkspaceMember> {
    Ok(WorkspaceMember {
        workspace_id: parse(row.try_get("workspace_id")?)?,
        member_id: parse(row.try_get("member_id")?)?,
        role: parse(row.try_get("role")?)?,
        created_at: parse_ts(row.try_get("created_at")?)?,
    })
}

#[async_trait]
impl MembershipRepo for Tx {
    async fn memberships_of(&mut self, member: MemberId) -> Result<Vec<WorkspaceMember>> {
        sqlx::query(
            "SELECT workspace_id, member_id, role, created_at FROM workspace_member
             WHERE member_id = $1 ORDER BY workspace_id",
        )
        .bind(member.to_string())
        .fetch_all(self.conn())
        .await?
        .iter()
        .map(membership_from)
        .collect()
    }

    async fn members_of(&mut self, workspace: WorkspaceId) -> Result<Vec<WorkspaceMember>> {
        sqlx::query(
            "SELECT workspace_id, member_id, role, created_at FROM workspace_member
             WHERE workspace_id = $1 ORDER BY member_id",
        )
        .bind(workspace.to_string())
        .fetch_all(self.conn())
        .await?
        .iter()
        .map(membership_from)
        .collect()
    }

    async fn role_of(&mut self, workspace: WorkspaceId, member: MemberId) -> Result<Option<Role>> {
        let role: Option<String> = sqlx::query_scalar(
            "SELECT role FROM workspace_member WHERE workspace_id = $1 AND member_id = $2",
        )
        .bind(workspace.to_string())
        .bind(member.to_string())
        .fetch_optional(self.conn())
        .await?;
        role.map(parse).transpose()
    }

    async fn add_member(
        &mut self,
        workspace: WorkspaceId,
        member: MemberId,
        role: Role,
    ) -> Result<WorkspaceMember> {
        let joined = WorkspaceMember {
            workspace_id: workspace,
            member_id: member,
            role,
            created_at: chrono::Utc::now(),
        };
        let done = sqlx::query(
            "INSERT INTO workspace_member (workspace_id, member_id, role, created_at)
             SELECT w.id, m.id, $3, $4 FROM workspace w, member m WHERE w.id = $1 AND m.id = $2",
        )
        .bind(workspace.to_string())
        .bind(member.to_string())
        .bind(role.as_str())
        .bind(ts(&joined.created_at))
        .execute(self.conn())
        .await;
        match done {
            Ok(done) if done.rows_affected() == 0 => {
                Err(StoreError::not_found("workspace or member", workspace))
            }
            Ok(_) => Ok(joined),
            Err(sqlx::Error::Database(db)) if db.is_unique_violation() => Err(
                sun_core::Error::InvalidInput("They are already a member of the workspace.".into())
                    .into(),
            ),
            Err(e) => Err(e.into()),
        }
    }

    async fn set_role(
        &mut self,
        workspace: WorkspaceId,
        member: MemberId,
        role: Role,
    ) -> Result<()> {
        if role != Role::Owner {
            not_last_owner(self, workspace, member).await?;
        }
        let done = sqlx::query(
            "UPDATE workspace_member SET role = $3 WHERE workspace_id = $1 AND member_id = $2",
        )
        .bind(workspace.to_string())
        .bind(member.to_string())
        .bind(role.as_str())
        .execute(self.conn())
        .await?;
        match done.rows_affected() {
            0 => Err(StoreError::not_found("workspace member", member)),
            _ => Ok(()),
        }
    }

    async fn remove_member(&mut self, workspace: WorkspaceId, member: MemberId) -> Result<bool> {
        not_last_owner(self, workspace, member).await?;
        let done =
            sqlx::query("DELETE FROM workspace_member WHERE workspace_id = $1 AND member_id = $2")
                .bind(workspace.to_string())
                .bind(member.to_string())
                .execute(self.conn())
                .await?;
        Ok(done.rows_affected() > 0)
    }

    async fn owner_count(&mut self, workspace: WorkspaceId) -> Result<i64> {
        Ok(sqlx::query_scalar(
            "SELECT count(*) FROM workspace_member WHERE workspace_id = $1 AND role = 'owner'",
        )
        .bind(workspace.to_string())
        .fetch_one(self.conn())
        .await?)
    }
}

const INVITATION_COLUMNS: &str = "id, workspace_id, email, role, inviter_id, created_at, \
     expires_at, accepted_at, declined_at";
/// Neither accepted nor declined, and not expired at `$1`.
const PENDING: &str = "accepted_at IS NULL AND declined_at IS NULL AND expires_at > $1";

fn invitation_from(row: &SqliteRow) -> Result<WorkspaceInvitation> {
    let at = |column: &str| -> Result<Option<Timestamp>> {
        row.try_get::<Option<String>, _>(column)?
            .map(parse_ts)
            .transpose()
    };
    Ok(WorkspaceInvitation {
        id: parse(row.try_get("id")?)?,
        workspace_id: parse(row.try_get("workspace_id")?)?,
        email: row.try_get("email")?,
        role: parse(row.try_get("role")?)?,
        inviter_id: row
            .try_get::<Option<String>, _>("inviter_id")?
            .map(parse)
            .transpose()?,
        created_at: parse_ts(row.try_get("created_at")?)?,
        expires_at: parse_ts(row.try_get("expires_at")?)?,
        accepted_at: at("accepted_at")?,
        declined_at: at("declined_at")?,
    })
}

/// Sets `column` (now) on a pending invitation; `false` when not pending.
async fn finish(tx: &mut Tx, id: InvitationId, column: &str) -> Result<bool> {
    let now = second(&chrono::Utc::now());
    // `column` is a literal from this module.
    let done = sqlx::query(&format!(
        "UPDATE workspace_invitation SET {column} = $1 WHERE id = $2 AND {PENDING}"
    ))
    .bind(&now)
    .bind(id.to_string())
    .execute(tx.conn())
    .await?;
    Ok(done.rows_affected() > 0)
}

#[async_trait]
impl InvitationRepo for Tx {
    async fn insert_invitation(
        &mut self,
        invitation: &WorkspaceInvitation,
        token_hash: &str,
    ) -> Result<()> {
        sqlx::query(
            "DELETE FROM workspace_invitation WHERE workspace_id = $1 AND lower(email) = lower($2)
             AND accepted_at IS NULL AND declined_at IS NULL",
        )
        .bind(invitation.workspace_id.to_string())
        .bind(&invitation.email)
        .execute(self.conn())
        .await?;
        sqlx::query(&format!(
            "INSERT INTO workspace_invitation ({INVITATION_COLUMNS}, token_hash)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)"
        ))
        .bind(invitation.id.to_string())
        .bind(invitation.workspace_id.to_string())
        .bind(invitation.email.to_ascii_lowercase())
        .bind(invitation.role.as_str())
        .bind(invitation.inviter_id.map(|m| m.to_string()))
        .bind(ts(&invitation.created_at))
        .bind(second(&invitation.expires_at))
        .bind(invitation.accepted_at.as_ref().map(second))
        .bind(invitation.declined_at.as_ref().map(second))
        .bind(token_hash)
        .execute(self.conn())
        .await?;
        Ok(())
    }

    async fn invitation_by_token_hash(
        &mut self,
        token_hash: &str,
    ) -> Result<Option<WorkspaceInvitation>> {
        sqlx::query(&format!(
            "SELECT {INVITATION_COLUMNS} FROM workspace_invitation WHERE token_hash = $1"
        ))
        .bind(token_hash)
        .fetch_optional(self.conn())
        .await?
        .as_ref()
        .map(invitation_from)
        .transpose()
    }

    async fn get_invitation(&mut self, id: InvitationId) -> Result<WorkspaceInvitation> {
        let row = sqlx::query(&format!(
            "SELECT {INVITATION_COLUMNS} FROM workspace_invitation WHERE id = $1"
        ))
        .bind(id.to_string())
        .fetch_optional(self.conn())
        .await?
        .ok_or_else(|| StoreError::not_found("invitation", id))?;
        invitation_from(&row)
    }

    async fn invitations(&mut self, workspace: WorkspaceId) -> Result<Vec<WorkspaceInvitation>> {
        sqlx::query(&format!(
            "SELECT {INVITATION_COLUMNS} FROM workspace_invitation
             WHERE workspace_id = $2 AND {PENDING} ORDER BY id DESC"
        ))
        .bind(second(&chrono::Utc::now()))
        .bind(workspace.to_string())
        .fetch_all(self.conn())
        .await?
        .iter()
        .map(invitation_from)
        .collect()
    }

    async fn invitations_by_email(&mut self, email: &str) -> Result<Vec<WorkspaceInvitation>> {
        sqlx::query(&format!(
            "SELECT {INVITATION_COLUMNS} FROM workspace_invitation
             WHERE lower(email) = lower($2) AND {PENDING} ORDER BY id DESC"
        ))
        .bind(second(&chrono::Utc::now()))
        .bind(email)
        .fetch_all(self.conn())
        .await?
        .iter()
        .map(invitation_from)
        .collect()
    }

    async fn accept_invitation(&mut self, id: InvitationId) -> Result<bool> {
        finish(self, id, "accepted_at").await
    }

    async fn decline_invitation(&mut self, id: InvitationId) -> Result<bool> {
        finish(self, id, "declined_at").await
    }

    async fn expire_invitation(&mut self, id: InvitationId) -> Result<bool> {
        finish(self, id, "expires_at").await
    }
}
