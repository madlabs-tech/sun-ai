//! Member routes: the deployment's accounts, the deployment owner flag
//! and the audit log (deployment owners, gated in `routes.rs`), and a
//! workspace's members: list (any member), change a role and remove
//! (`edit_members` there, never above your own role), leave (yourself).
//! A workspace keeps an owner: the change that would leave it none is 409.

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::{Extension, Json};
use serde::{Deserialize, Serialize};
use sun_core::model::{AuditEntry, Member, Permission, Role, WorkspaceMember};
use sun_core::{AuditId, Error, MemberId, Timestamp, WorkspaceId};

use super::Caller;
use crate::Api;
use crate::error::{ApiError, ApiResult};
use crate::routes::Body;

/// Audit entries per page, at most.
const MAX_AUDIT_PAGE: u32 = 500;

/// `GET /api/v1/members`
pub(crate) async fn list_members(State(api): State<Api>) -> ApiResult<Json<Vec<Member>>> {
    Ok(Json(api.engine.members().await?))
}

/// 422 for an id that doesn't parse.
fn parse<T: std::str::FromStr>(id: &str, what: &str) -> ApiResult<T> {
    id.parse()
        .map_err(|_| ApiError::invalid(format!("`{id}` is not a valid {what} id.")))
}

/// The privilege-escalation guard for putting someone in `workspace` as
/// `role` with `permission` (inviting or adding them): a deployment owner
/// may; anyone else needs a role there that grants `permission` and is
/// `role` or above it (403 otherwise).
pub(super) fn may_grant(
    caller: &Caller,
    workspace: WorkspaceId,
    role: Role,
    permission: Permission,
) -> ApiResult<()> {
    let mine = caller.role_in(workspace);
    if caller.is_owner
        || mine.is_some_and(|r| r.covers(role) && r.permissions().contains(&permission))
    {
        return Ok(());
    }
    Err(ApiError::new(
        StatusCode::FORBIDDEN,
        "forbidden",
        format!("You can't add someone as {role} in that workspace."),
    ))
}

/// The admin routes that reset how someone signs in (password, reset code,
/// 2-step reset) refuse the caller's own account, owners included (403):
/// `/me/*` asks for the current password and the second step, they don't.
pub(super) fn not_yourself(caller: &Caller, id: MemberId) -> ApiResult<()> {
    if caller.member_id == Some(id) {
        return Err(ApiError::new(
            StatusCode::FORBIDDEN,
            "forbidden",
            "Use your account settings to change your own password or 2-step.",
        ));
    }
    Ok(())
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SetOwner {
    is_owner: bool,
}

/// `PUT /api/v1/members/{id}/owner {is_owner}` (deployment owner): makes
/// them a deployment owner or not, audited. Nobody changes their own.
pub(crate) async fn set_owner(
    State(api): State<Api>,
    Extension(caller): Extension<Caller>,
    Path(id): Path<String>,
    Body(input): Body<SetOwner>,
) -> ApiResult<Json<Member>> {
    let id: MemberId = parse(&id, "member")?;
    if caller.member_id == Some(id) {
        return Err(ApiError::invalid("You can't change your own owner flag."));
    }
    Ok(Json(
        api.engine
            .set_member_owner(id, input.is_owner, &caller.actor())
            .await?,
    ))
}

#[derive(Deserialize)]
pub(crate) struct AuditPage {
    before: Option<String>,
    limit: Option<u32>,
}

/// `GET /api/v1/audit[?before=<id>&limit=n]`: newest first.
pub(crate) async fn audit_log(
    State(api): State<Api>,
    Query(page): Query<AuditPage>,
) -> ApiResult<Json<Vec<AuditEntry>>> {
    let before = page
        .before
        .map(|b| b.parse::<AuditId>())
        .transpose()
        .map_err(|_| ApiError::invalid("`before` is not a valid audit id."))?;
    let limit = page.limit.unwrap_or(100).clamp(1, MAX_AUDIT_PAGE);
    Ok(Json(api.engine.audit_log(before, limit).await?))
}

/// One member of a workspace, with who they are.
#[derive(Serialize)]
pub(crate) struct WorkspaceMemberView {
    member_id: MemberId,
    workspace_id: WorkspaceId,
    role: Role,
    username: String,
    email: Option<String>,
    /// When they joined the workspace.
    created_at: Timestamp,
}

/// `GET /api/v1/workspaces/{id}/members` (any member of it): oldest first.
pub(crate) async fn workspace_members(
    State(api): State<Api>,
    Path(workspace): Path<String>,
) -> ApiResult<Json<Vec<WorkspaceMemberView>>> {
    let workspace: WorkspaceId = parse(&workspace, "workspace")?;
    let people = api.engine.members().await?;
    let views = api
        .engine
        .workspace_members(workspace)
        .await?
        .into_iter()
        .filter_map(|m| {
            let person = people.iter().find(|p| p.id == m.member_id)?;
            Some(WorkspaceMemberView {
                member_id: m.member_id,
                workspace_id: m.workspace_id,
                role: m.role,
                username: person.username.clone(),
                email: person.email.clone(),
                created_at: m.created_at,
            })
        })
        .collect();
    Ok(Json(views))
}

/// A change that would leave a workspace no owner is a conflict (409).
fn last_owner(error: Error) -> ApiError {
    match error {
        Error::InvalidInput(message) => ApiError::conflict(message),
        e => e.into(),
    }
}

/// 403 unless the caller may change `target`'s membership in `workspace`
/// (to `role`, when given): a deployment owner may; anyone else's role
/// there must be `target`'s current role or above it, and `role` or above
/// it (so only an owner touches or makes an owner). 404 when `target`
/// isn't a member.
async fn may_change(
    api: &Api,
    caller: &Caller,
    workspace: WorkspaceId,
    target: MemberId,
    role: Option<Role>,
) -> ApiResult<()> {
    let current = api
        .engine
        .workspace_members(workspace)
        .await?
        .into_iter()
        .find(|m| m.member_id == target)
        .map(|m| m.role)
        .ok_or_else(|| Error::NotFound(format!("workspace member {target}")))?;
    if caller.is_owner {
        return Ok(());
    }
    let mine = caller.role_in(workspace);
    let covers = |r: Role| mine.is_some_and(|m| m.covers(r));
    if covers(current) && role.is_none_or(covers) {
        return Ok(());
    }
    Err(ApiError::new(
        StatusCode::FORBIDDEN,
        "forbidden",
        "You can't change the role of someone above you, or give a role above your own.",
    ))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SetRole {
    role: Role,
}

/// `PUT /api/v1/workspaces/{id}/members/{member} {role}` (`edit_members`
/// there): their new role, audited. Never above the caller's own role, nor
/// on someone above it; 409 when it would leave the workspace no owner.
pub(crate) async fn set_workspace_role(
    State(api): State<Api>,
    Extension(caller): Extension<Caller>,
    Path((workspace, member)): Path<(String, String)>,
    Body(input): Body<SetRole>,
) -> ApiResult<Json<WorkspaceMember>> {
    let workspace: WorkspaceId = parse(&workspace, "workspace")?;
    let member: MemberId = parse(&member, "member")?;
    may_change(&api, &caller, workspace, member, Some(input.role)).await?;
    let changed = api
        .engine
        .set_role(workspace, member, input.role, &caller.actor())
        .await
        .map_err(last_owner)?;
    Ok(Json(changed))
}

/// `DELETE /api/v1/workspaces/{id}/members/{member}` (`edit_members`
/// there): takes them out of the workspace, audited. Not someone above the
/// caller's role; 409 for its last owner.
pub(crate) async fn remove_workspace_member(
    State(api): State<Api>,
    Extension(caller): Extension<Caller>,
    Path((workspace, member)): Path<(String, String)>,
) -> ApiResult<StatusCode> {
    let workspace: WorkspaceId = parse(&workspace, "workspace")?;
    let member: MemberId = parse(&member, "member")?;
    may_change(&api, &caller, workspace, member, None).await?;
    api.engine
        .remove_member(workspace, member, &caller.actor())
        .await
        .map_err(last_owner)?;
    Ok(StatusCode::NO_CONTENT)
}

/// `POST /api/v1/workspaces/{id}/leave` (any member of it): the caller
/// leaves, audited; 409 for its last owner (make someone else owner first).
pub(crate) async fn leave_workspace(
    State(api): State<Api>,
    Extension(caller): Extension<Caller>,
    Path(workspace): Path<String>,
) -> ApiResult<StatusCode> {
    let workspace: WorkspaceId = parse(&workspace, "workspace")?;
    let Some(id) = caller.member_id else {
        return Err(ApiError::invalid(
            "The kernel token holder is in no workspace to leave.",
        ));
    };
    api.engine.leave(workspace, id).await.map_err(last_owner)?;
    Ok(StatusCode::NO_CONTENT)
}
