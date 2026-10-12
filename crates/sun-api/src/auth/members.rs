//! Member and group routes: members, their groups and
//! the groups themselves, and the audit log. Each is
//! gated in `routes.rs` by its own permission (`view_members`,
//! `edit_members`, `manage_groups`, `manage_settings` for the audit log).

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::{Extension, Json};
use serde::Deserialize;
use sun_core::model::{AuditEntry, Group, Member, Permission};
use sun_core::{AuditId, GroupId, MemberId};

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
            "Only an owner can invite, re-invite, remove or regroup an owner.",
        ));
    }
    Ok(())
}

/// The privilege-escalation guard for every route that puts people in
/// groups or takes them out (invites, `PUT /members/{id}/groups`,
/// `PUT /groups/{id}/members`): without `manage_groups` (an owner has it),
/// a caller may add or remove only groups whose permissions they hold
/// themselves (403 otherwise). `changed` are the groups added or removed;
/// unknown ids are left to the engine's 404.
pub(super) async fn may_grant(
    api: &Api,
    caller: &Caller,
    changed: impl IntoIterator<Item = GroupId>,
) -> ApiResult<()> {
    let changed: Vec<GroupId> = changed.into_iter().collect();
    if caller.can(Permission::ManageGroups) || changed.is_empty() {
        return Ok(());
    }
    let groups = api.engine.groups().await?;
    let beyond = groups
        .iter()
        .filter(|g| changed.contains(&g.id))
        .any(|g| g.permissions.iter().any(|p| !caller.can(*p)));
    if beyond {
        return Err(ApiError::new(
            StatusCode::FORBIDDEN,
            "forbidden",
            "You can't grant permissions you don't have. Ask someone with the \
             `manage_groups` permission.",
        ));
    }
    // A group also opens the workspaces attached to it.
    if caller.workspaces.is_some() {
        let hidden = api
            .engine
            .workspace_groups()
            .await?
            .into_iter()
            .any(|(workspace, group)| changed.contains(&group) && !caller.sees(workspace));
        if hidden {
            return Err(ApiError::new(
                StatusCode::FORBIDDEN,
                "forbidden",
                "That group opens a workspace you can't see. Ask someone with the \
                 `manage_groups` permission.",
            ));
        }
    }
    Ok(())
}

/// The take-over guard for every route that acts on another member's
/// account (password, reset code, 2-step reset, deactivate / activate,
/// groups): only an owner touches an owner, and without
/// `manage_groups` (an owner has it) the target may hold no permission
/// and see no workspace the caller doesn't (403 otherwise). Returns the
/// member; unknown ids are 404.
pub(super) async fn may_act_on(api: &Api, caller: &Caller, id: MemberId) -> ApiResult<Member> {
    let target = api.engine.member_access(id).await?;
    only_owner_touches_owner(caller, target.member.is_owner)?;
    if caller.can(Permission::ManageGroups) {
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
                 account. Ask someone with the `manage_groups` permission.",
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
pub(crate) struct SetGroups {
    group_ids: Vec<String>,
}

/// `PUT /api/v1/members/{id}/groups {group_ids}`: puts the member in
/// exactly these groups, audited. The member must pass [`may_act_on`];
/// without `manage_groups` nobody changes their own groups, and the groups
/// added or removed must pass [`may_grant`].
pub(crate) async fn set_member_groups(
    State(api): State<Api>,
    Extension(caller): Extension<Caller>,
    Path(id): Path<String>,
    Body(input): Body<SetGroups>,
) -> ApiResult<Json<Member>> {
    let id: MemberId = parse(&id, "member")?;
    let groups = input
        .group_ids
        .iter()
        .map(|g| parse(g, "group"))
        .collect::<ApiResult<Vec<GroupId>>>()?;
    let member = may_act_on(&api, &caller, id).await?;
    let changed: Vec<GroupId> = groups
        .iter()
        .filter(|g| !member.groups.contains(g))
        .chain(member.groups.iter().filter(|g| !groups.contains(g)))
        .copied()
        .collect();
    let own = caller.member_id == Some(id);
    if own && !changed.is_empty() && !caller.can(Permission::ManageGroups) {
        return Err(ApiError::new(
            StatusCode::FORBIDDEN,
            "forbidden",
            "You can't change your own groups. Ask someone with the `manage_groups` permission.",
        ));
    }
    may_grant(&api, &caller, changed).await?;
    Ok(Json(
        api.engine
            .set_member_groups(id, &groups, &caller.actor())
            .await?,
    ))
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

/// `GET /api/v1/groups`: every group, oldest first.
pub(crate) async fn list_groups(State(api): State<Api>) -> ApiResult<Json<Vec<Group>>> {
    Ok(Json(api.engine.groups().await?))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct NewGroup {
    name: String,
    #[serde(default)]
    permissions: Vec<Permission>,
}

/// `POST /api/v1/groups {name, permissions?}`: a new group (201), keyed
/// from its name. A taken name is 422.
pub(crate) async fn create_group(
    State(api): State<Api>,
    Extension(caller): Extension<Caller>,
    Body(input): Body<NewGroup>,
) -> ApiResult<(StatusCode, Json<Group>)> {
    let group = api
        .engine
        .create_group(&input.name, &input.permissions, &caller.actor())
        .await?;
    Ok((StatusCode::CREATED, Json(group)))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct GroupChange {
    name: Option<String>,
    permissions: Option<Vec<Permission>>,
}

/// `PATCH /api/v1/groups/{id} {name?, permissions?}`: renames (the key
/// stays) and/or replaces the permissions, audited.
pub(crate) async fn update_group(
    State(api): State<Api>,
    Extension(caller): Extension<Caller>,
    Path(id): Path<String>,
    Body(input): Body<GroupChange>,
) -> ApiResult<Json<Group>> {
    let id: GroupId = parse(&id, "group")?;
    let group = api
        .engine
        .update_group(
            id,
            input.name.as_deref(),
            input.permissions.as_deref(),
            &caller.actor(),
        )
        .await?;
    Ok(Json(group))
}

/// `DELETE /api/v1/groups/{id}`: deletes the group; its members leave it.
pub(crate) async fn delete_group(
    State(api): State<Api>,
    Extension(caller): Extension<Caller>,
    Path(id): Path<String>,
) -> ApiResult<StatusCode> {
    let id: GroupId = parse(&id, "group")?;
    api.engine.delete_group(id, &caller.actor()).await?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SetMembers {
    member_ids: Vec<String>,
}

/// `PUT /api/v1/groups/{id}/members {member_ids}`: makes these exactly the
/// group's members, audited. Only an owner adds or drops an owner.
pub(crate) async fn set_group_members(
    State(api): State<Api>,
    Extension(caller): Extension<Caller>,
    Path(id): Path<String>,
    Body(input): Body<SetMembers>,
) -> ApiResult<Json<Vec<Member>>> {
    let id: GroupId = parse(&id, "group")?;
    let wanted = input
        .member_ids
        .iter()
        .map(|m| parse(m, "member"))
        .collect::<ApiResult<Vec<MemberId>>>()?;
    if !caller.is_owner {
        // An owner whose membership would change.
        let touched = api
            .engine
            .members()
            .await?
            .into_iter()
            .any(|m| m.is_owner && m.groups.contains(&id) != wanted.contains(&m.id));
        only_owner_touches_owner(&caller, touched)?;
    }
    // Always passes today (the route needs manage_groups); kept so a
    // re-gate can't open an escalation.
    may_grant(&api, &caller, [id]).await?;
    Ok(Json(
        api.engine
            .set_group_members(id, &wanted, &caller.actor())
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
