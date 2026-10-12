//! Member routes: members, the deployment owner flag, and the audit log.
//! Each is gated in `routes.rs` by its own permission (`view_members`,
//! `edit_members`, `manage_settings` for the audit log). Workspace roles
//! and invitations get their own routes in M2.

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::{Extension, Json};
use serde::Deserialize;
use sun_core::model::{AuditEntry, Member, Permission, Role};
use sun_core::{AuditId, MemberId, WorkspaceId};

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

/// 403 unless an owner does it, when `target` is an owner.
pub(super) fn only_owner_touches_owner(caller: &Caller, target_is_owner: bool) -> ApiResult<()> {
    if !caller.is_owner && target_is_owner {
        return Err(ApiError::new(
            StatusCode::FORBIDDEN,
            "forbidden",
            "Only an owner can invite, re-invite or remove an owner.",
        ));
    }
    Ok(())
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

/// The take-over guard for every route that acts on another member's
/// account (password, reset code, 2-step reset, deactivate / activate,
/// role): only an owner touches an owner, and anyone else's target may
/// hold no permission and see no workspace the caller doesn't (403
/// otherwise). Returns the member; unknown ids are 404.
pub(super) async fn may_act_on(api: &Api, caller: &Caller, id: MemberId) -> ApiResult<Member> {
    let target = api.engine.member_access(id).await?;
    only_owner_touches_owner(caller, target.member.is_owner)?;
    if caller.is_owner {
        return Ok(target.member);
    }
    let stronger = target.permissions.iter().any(|p| !caller.can(*p));
    let wider = match &target.workspaces {
        None => caller.workspaces.is_some(),
        Some(workspaces) => workspaces.iter().any(|b| !caller.sees(*b)),
    };
    if stronger || wider {
        return Err(ApiError::new(
            StatusCode::FORBIDDEN,
            "forbidden",
            format!(
                "{} can do more or see more workspaces than you, so you can't change their \
                 account. Ask a deployment owner.",
                target.member.username
            ),
        ));
    }
    Ok(target.member)
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

/// `PUT /api/v1/members/{id}/owner {is_owner}` (owner): grants or takes
/// back owner, audited. Nobody changes their own.
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
