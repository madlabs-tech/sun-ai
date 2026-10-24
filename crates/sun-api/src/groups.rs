//! Groups and the terminal policy (BLO-192). Deployment owners manage the
//! groups ([`sun_core::group`]) and the [`TerminalPolicy`] on the
//! owner-only router; any member reads their own groups and whether they
//! may open terminals (`GET /me` `can_terminal`).
//!
//! - `GET /groups`, `POST /groups {name, description?, domains?}`,
//!   `PATCH /groups/{id} {name?, description?, domains?}`,
//!   `DELETE /groups/{id}`.
//! - `POST /groups/{id}/members/{member}`, `DELETE` it: manual members.
//! - `GET /groups/{id}/preview?domains=a.io,b.io`: who those domains (the
//!   group's own when none are given) would bring in.
//! - `GET /settings/terminal`, `PUT /settings/terminal {mode, groups}`.
//! - `GET /groups/mine`: any member's own groups.

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::{Extension, Json};
use serde::Deserialize;
use serde_json::{Value, json};
use sun_core::group::{MemberGroup, TerminalPolicy};
use sun_core::model::Member;
use sun_core::{GroupId, MemberId};
use sun_engine::GroupChange;

use crate::Api;
use crate::auth::Caller;
use crate::error::ApiResult;
use crate::routes::{Body, id};

/// May `caller` open terminals, their grants aside: the terminal policy
/// with their groups now (a deployment owner always may).
pub(crate) async fn can_terminal(api: &Api, caller: &Caller) -> ApiResult<bool> {
    let (groups, policy) = api.engine.terminal_access(caller.member_id).await?;
    Ok(policy.allows(caller.is_owner, &groups))
}

/// A group with its manual members.
fn view(group: MemberGroup, member_ids: &[MemberId]) -> Value {
    let mut out = json!(group);
    out["member_ids"] = json!(member_ids);
    out
}

/// `GET /api/v1/groups` (deployment owner): every group with its manual
/// `member_ids`, by name.
pub(crate) async fn list(State(api): State<Api>) -> ApiResult<Json<Vec<Value>>> {
    let groups = api.engine.groups().await?;
    Ok(Json(groups.into_iter().map(|(g, m)| view(g, &m)).collect()))
}

/// The body of `POST /groups`.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct NewGroup {
    name: String,
    #[serde(default)]
    description: String,
    #[serde(default)]
    domains: Vec<String>,
}

/// `POST /api/v1/groups {name, description?, domains?}` (deployment owner):
/// 201 with the group, audited; 422 for a bad or taken name or a bad
/// domain.
pub(crate) async fn create(
    State(api): State<Api>,
    Extension(caller): Extension<Caller>,
    Body(input): Body<NewGroup>,
) -> ApiResult<(StatusCode, Json<Value>)> {
    let group = api
        .engine
        .create_group(
            &input.name,
            &input.description,
            &input.domains,
            caller.member_id,
            &caller.actor(),
        )
        .await?;
    Ok((StatusCode::CREATED, Json(view(group, &[]))))
}

/// The body of `PATCH /groups/{id}`.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct EditGroup {
    name: Option<String>,
    description: Option<String>,
    domains: Option<Vec<String>>,
}

/// `PATCH /api/v1/groups/{id} {name?, description?, domains?}` (deployment
/// owner): the group, audited; a membership it changes applies to the next
/// request.
pub(crate) async fn update(
    State(api): State<Api>,
    Extension(caller): Extension<Caller>,
    Path(group): Path<String>,
    Body(input): Body<EditGroup>,
) -> ApiResult<Json<Value>> {
    let group: GroupId = id(&group, "group")?;
    let change = GroupChange {
        name: input.name,
        description: input.description,
        domains: input.domains,
    };
    api.engine
        .update_group(group, change, &caller.actor())
        .await?;
    let (group, member_ids) = api.engine.group(group).await?;
    Ok(Json(view(group, &member_ids)))
}

/// `DELETE /api/v1/groups/{id}` (deployment owner): 204, audited; its
/// editor grants go with it and the terminal policy stops listing it.
pub(crate) async fn remove(
    State(api): State<Api>,
    Extension(caller): Extension<Caller>,
    Path(group): Path<String>,
) -> ApiResult<StatusCode> {
    let group: GroupId = id(&group, "group")?;
    api.engine.delete_group(group, &caller.actor()).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// `POST /api/v1/groups/{id}/members/{member}` (deployment owner): 204,
/// audited; 404 for an unknown group or member.
pub(crate) async fn add_member(
    State(api): State<Api>,
    Extension(caller): Extension<Caller>,
    Path((group, member)): Path<(String, String)>,
) -> ApiResult<StatusCode> {
    let group: GroupId = id(&group, "group")?;
    let member: MemberId = id(&member, "member")?;
    api.engine
        .add_group_member(group, member, caller.member_id, &caller.actor())
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

/// `DELETE /api/v1/groups/{id}/members/{member}` (deployment owner): 204,
/// audited; 404 when they weren't added by hand.
pub(crate) async fn remove_member(
    State(api): State<Api>,
    Extension(caller): Extension<Caller>,
    Path((group, member)): Path<(String, String)>,
) -> ApiResult<StatusCode> {
    let group: GroupId = id(&group, "group")?;
    let member: MemberId = id(&member, "member")?;
    api.engine
        .remove_group_member(group, member, &caller.actor())
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

/// The query of `GET /groups/{id}/preview`.
#[derive(Deserialize)]
pub(crate) struct Preview {
    /// Comma-separated; none: the group's own.
    domains: Option<String>,
}

/// `GET /api/v1/groups/{id}/preview?domains=` (deployment owner): the
/// active members whose verified email is on one of `domains`.
pub(crate) async fn preview(
    State(api): State<Api>,
    Path(group): Path<String>,
    Query(query): Query<Preview>,
) -> ApiResult<Json<Vec<Member>>> {
    let group: GroupId = id(&group, "group")?;
    let (saved, _) = api.engine.group(group).await?;
    let domains = match query.domains {
        Some(list) => list
            .split(',')
            .filter(|d| !d.trim().is_empty())
            .map(str::to_owned)
            .collect(),
        None => saved.domains,
    };
    Ok(Json(api.engine.group_preview(&domains).await?))
}

/// `GET /api/v1/groups/mine` (any member): the caller's groups now, by
/// name (none for the token holder).
pub(crate) async fn mine(
    State(api): State<Api>,
    Extension(caller): Extension<Caller>,
) -> ApiResult<Json<Vec<MemberGroup>>> {
    Ok(Json(match caller.member_id {
        Some(member) => api.engine.groups_of(member).await?,
        None => Vec::new(),
    }))
}

/// `GET /api/v1/settings/terminal` (deployment owner): `{mode, groups}`.
pub(crate) async fn get_policy(State(api): State<Api>) -> ApiResult<Json<TerminalPolicy>> {
    Ok(Json(api.engine.terminal_policy().await?))
}

/// `PUT /api/v1/settings/terminal {mode, groups}` (deployment owner): saves
/// it, audited; every listed group must exist.
pub(crate) async fn put_policy(
    State(api): State<Api>,
    Extension(caller): Extension<Caller>,
    Body(policy): Body<TerminalPolicy>,
) -> ApiResult<Json<TerminalPolicy>> {
    let policy = api
        .engine
        .set_terminal_policy(policy, &caller.actor())
        .await?;
    Ok(Json(policy))
}
