//! `/workspaces`: lists the workspaces the caller sees, each with the groups that
//! see it; create needs `create_workspace`; the profile (`PATCH`: name,
//! description, context, default workflow, `git_settings`; logo; issue
//! prefix) and access (`PUT /workspaces/{id}/groups`) `edit_workspace`;
//! delete `delete_workspace` (layers in `routes.rs`). Anyone who sees a
//! workspace may read its logo. Without `manage_groups`, a caller attaches or detaches
//! only their own groups. Importing a knowledge source into another
//! workspace (and removing a source) needs `manage_settings`, removing a
//! repo `edit_workspace`;
//! [`crate::workspace_access::guard`] makes
//! the importer see both workspaces. Every write is audited by the engine.

use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::{Extension, Json};
use serde::{Deserialize, Serialize};
use sun_core::model::{Permission, Workspace};
use sun_core::{GroupId, RepoId, WorkspaceId};
use sun_engine::{ProfileChange, WorkspaceItem};

use crate::Api;
use crate::auth::Caller;
use crate::error::{ApiError, ApiResult};
use crate::routes::{Body, id};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct IssuePrefix {
    issue_prefix: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct NewWorkspace {
    name: String,
    /// Default: from the name, made unique.
    #[serde(default)]
    slug: Option<String>,
    /// The groups that see it; none: admins only.
    #[serde(default)]
    group_ids: Vec<GroupId>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct WorkspaceGroups {
    group_ids: Vec<GroupId>,
}

/// A workspace with the groups that see it.
#[derive(Serialize)]
pub(crate) struct WorkspaceView {
    #[serde(flatten)]
    workspace: Workspace,
    group_ids: Vec<GroupId>,
    /// `GET /workspaces/{id}/logo`, versioned by the image; `null`: no logo.
    logo_url: Option<String>,
}

async fn view(api: &Api, workspace: Workspace) -> ApiResult<WorkspaceView> {
    let links = api.engine.workspace_groups().await?;
    Ok(with_groups(workspace, &links))
}

fn with_groups(workspace: Workspace, links: &[(WorkspaceId, GroupId)]) -> WorkspaceView {
    let group_ids = links
        .iter()
        .filter(|(b, _)| *b == workspace.id)
        .map(|(_, g)| *g)
        .collect();
    let logo_url = workspace.logo_hash.as_ref().map(|hash| {
        let version = hash.get(..16).unwrap_or(hash);
        format!("/api/v1/workspaces/{}/logo?v={version}", workspace.id)
    });
    WorkspaceView {
        workspace,
        group_ids,
        logo_url,
    }
}

/// Without `manage_groups`, a caller may attach or detach only groups they
/// are in (422 otherwise).
fn only_own_groups(caller: &Caller, changed: impl Iterator<Item = GroupId>) -> ApiResult<()> {
    if caller.can(Permission::ManageGroups) {
        return Ok(());
    }
    let mine = |g: &GroupId| caller.groups.iter().any(|own| own.id == *g);
    match changed.filter(|g| !mine(g)).count() {
        0 => Ok(()),
        _ => Err(ApiError::invalid(
            "You can give or take workspace access only for your own groups.",
        )),
    }
}

/// The workspaces the caller sees, oldest first (Main first).
pub(crate) async fn list(
    State(api): State<Api>,
    Extension(caller): Extension<Caller>,
) -> ApiResult<Json<Vec<WorkspaceView>>> {
    let links = api.engine.workspace_groups().await?;
    let workspaces = api.engine.workspaces().await?;
    Ok(Json(
        workspaces
            .into_iter()
            .filter(|b| caller.sees(b.id))
            .map(|b| with_groups(b, &links))
            .collect(),
    ))
}

/// `POST /workspaces {name, group_ids?}`.
pub(crate) async fn create(
    State(api): State<Api>,
    Extension(caller): Extension<Caller>,
    Body(input): Body<NewWorkspace>,
) -> ApiResult<(StatusCode, Json<WorkspaceView>)> {
    only_own_groups(&caller, input.group_ids.iter().copied())?;
    let workspace = api
        .engine
        .create_workspace(
            &input.name,
            input.slug.as_deref(),
            &input.group_ids,
            &caller.actor(),
        )
        .await?;
    Ok((StatusCode::CREATED, Json(view(&api, workspace).await?)))
}

/// `PUT /workspaces/{id}/groups {group_ids}`: exactly these groups see it.
pub(crate) async fn set_groups(
    State(api): State<Api>,
    Extension(caller): Extension<Caller>,
    Path(workspace): Path<String>,
    Body(input): Body<WorkspaceGroups>,
) -> ApiResult<Json<WorkspaceView>> {
    let workspace: WorkspaceId = id(&workspace, "workspace")?;
    let before = api.engine.groups_of_workspace(workspace).await?;
    let changed = before
        .iter()
        .filter(|g| !input.group_ids.contains(g))
        .chain(input.group_ids.iter().filter(|g| !before.contains(g)))
        .copied();
    only_own_groups(&caller, changed)?;
    api.engine
        .set_workspace_groups(workspace, &input.group_ids, &caller.actor())
        .await?;
    Ok(Json(
        view(&api, api.engine.workspace(workspace).await?).await?,
    ))
}

/// `PATCH /workspaces/{id}` with any of `name`, `description`, `context`,
/// `default_workflow_id` (`null` clears it), `git_settings` (all four
/// switches).
pub(crate) async fn update(
    State(api): State<Api>,
    Extension(caller): Extension<Caller>,
    Path(workspace): Path<String>,
    Body(change): Body<ProfileChange>,
) -> ApiResult<Json<WorkspaceView>> {
    let workspace: WorkspaceId = id(&workspace, "workspace")?;
    let workspace = api
        .engine
        .update_workspace_profile(workspace, &change, &caller.actor())
        .await?;
    Ok(Json(view(&api, workspace).await?))
}

/// `POST /workspaces/{id}/prefix {issue_prefix}`.
pub(crate) async fn set_prefix(
    State(api): State<Api>,
    Extension(caller): Extension<Caller>,
    Path(workspace): Path<String>,
    Body(input): Body<IssuePrefix>,
) -> ApiResult<Json<WorkspaceView>> {
    let workspace: WorkspaceId = id(&workspace, "workspace")?;
    let workspace = api
        .engine
        .set_issue_prefix(workspace, &input.issue_prefix, &caller.actor())
        .await?;
    Ok(Json(view(&api, workspace).await?))
}

/// `PUT /workspaces/{id}/logo`: the raw image as the body, its type in
/// `Content-Type` (PNG, JPEG or WebP, at most 1 MB; the bytes must match).
pub(crate) async fn put_logo(
    State(api): State<Api>,
    Extension(caller): Extension<Caller>,
    Path(workspace): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> ApiResult<Json<WorkspaceView>> {
    let workspace: WorkspaceId = id(&workspace, "workspace")?;
    let content_type = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default();
    let workspace = api
        .engine
        .set_logo(
            workspace,
            Some((body.to_vec(), content_type)),
            &caller.actor(),
        )
        .await?;
    Ok(Json(view(&api, workspace).await?))
}

/// `DELETE /workspaces/{id}/logo`.
pub(crate) async fn delete_logo(
    State(api): State<Api>,
    Extension(caller): Extension<Caller>,
    Path(workspace): Path<String>,
) -> ApiResult<Json<WorkspaceView>> {
    let workspace: WorkspaceId = id(&workspace, "workspace")?;
    let workspace = api
        .engine
        .set_logo(workspace, None, &caller.actor())
        .await?;
    Ok(Json(view(&api, workspace).await?))
}

/// `GET /workspaces/{id}/logo`: the image; 404 when there is none.
pub(crate) async fn get_logo(
    State(api): State<Api>,
    Path(workspace): Path<String>,
) -> ApiResult<Response> {
    let (bytes, kind) = api.engine.logo(id(&workspace, "workspace")?).await?;
    Ok((
        [
            (header::CONTENT_TYPE, kind.as_str()),
            (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
            (header::CACHE_CONTROL, "private, no-cache"),
        ],
        bytes,
    )
        .into_response())
}

/// Deletes an empty workspace (422 while it has workflows, or for Main).
pub(crate) async fn remove(
    State(api): State<Api>,
    Extension(caller): Extension<Caller>,
    Path(workspace): Path<String>,
) -> ApiResult<StatusCode> {
    let workspace: WorkspaceId = id(&workspace, "workspace")?;
    api.engine
        .delete_workspace(workspace, &caller.actor())
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

/// `DELETE /workspaces/{id}/repos/{repo_id}` (`edit_workspace`): deletes
/// the repo from the workspace's list and the GitLab hook Sun made for it.
pub(crate) async fn remove_repo(
    State(api): State<Api>,
    Extension(caller): Extension<Caller>,
    Path((workspace, repo)): Path<(String, String)>,
) -> ApiResult<StatusCode> {
    api.engine
        .remove_workspace_repo(
            id(&workspace, "workspace")?,
            id::<RepoId>(&repo, "repo")?,
            &caller.actor(),
        )
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

/// `POST /workspaces/{id}/knowledge_sources/{source_id}`: links an existing
/// source to the workspace (the same index).
pub(crate) async fn import_knowledge_source(
    State(api): State<Api>,
    Extension(caller): Extension<Caller>,
    Path((workspace, source)): Path<(String, String)>,
) -> ApiResult<StatusCode> {
    let item = WorkspaceItem::KnowledgeSource(&source);
    api.engine
        .import_to_workspace(id(&workspace, "workspace")?, item, &caller.actor())
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

/// `DELETE /workspaces/{id}/knowledge_sources/{source_id}`: the source is
/// deleted when no workspace has it anymore.
pub(crate) async fn detach_knowledge_source(
    State(api): State<Api>,
    Extension(caller): Extension<Caller>,
    Path((workspace, source)): Path<(String, String)>,
) -> ApiResult<StatusCode> {
    let item = WorkspaceItem::KnowledgeSource(&source);
    api.engine
        .detach_from_workspace(id(&workspace, "workspace")?, item, &caller.actor())
        .await?;
    Ok(StatusCode::NO_CONTENT)
}
