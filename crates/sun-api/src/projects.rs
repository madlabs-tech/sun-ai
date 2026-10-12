//! Projects: `GET|POST /workspaces/{id}/projects`, `PATCH|DELETE /projects/{id}`.
//! Reads need access to the workspace, writes `run` too (`require_by_method`);
//! [`crate::workspace_access::guard`] answers 404 for a hidden workspace's.

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::{Extension, Json};
use serde::Deserialize;
use sun_core::model::Project;
use sun_core::{ProjectId, WorkspaceId};
use sun_engine::ProjectChange;

use crate::Api;
use crate::auth::Caller;
use crate::error::ApiResult;
use crate::routes::{Body, id};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct NewProject {
    name: String,
    #[serde(default)]
    description: String,
}

/// The workspace's projects, by name.
pub(crate) async fn list(
    State(api): State<Api>,
    Path(workspace): Path<String>,
) -> ApiResult<Json<Vec<Project>>> {
    let workspace: WorkspaceId = id(&workspace, "workspace")?;
    Ok(Json(api.engine.projects_of_workspace(workspace).await?))
}

/// `POST /workspaces/{id}/projects {name, description?}`.
pub(crate) async fn create(
    State(api): State<Api>,
    Extension(caller): Extension<Caller>,
    Path(workspace): Path<String>,
    Body(input): Body<NewProject>,
) -> ApiResult<(StatusCode, Json<Project>)> {
    let workspace: WorkspaceId = id(&workspace, "workspace")?;
    let project = api
        .engine
        .create_project(workspace, &input.name, &input.description, &caller.actor())
        .await?;
    Ok((StatusCode::CREATED, Json(project)))
}

/// `PATCH /projects/{id}` with any of `name`, `description`.
pub(crate) async fn update(
    State(api): State<Api>,
    Extension(caller): Extension<Caller>,
    Path(project): Path<String>,
    Body(change): Body<ProjectChange>,
) -> ApiResult<Json<Project>> {
    let project: ProjectId = id(&project, "project")?;
    Ok(Json(
        api.engine
            .update_project(project, &change, &caller.actor())
            .await?,
    ))
}

/// `DELETE /projects/{id}`.
pub(crate) async fn remove(
    State(api): State<Api>,
    Extension(caller): Extension<Caller>,
    Path(project): Path<String>,
) -> ApiResult<StatusCode> {
    let project: ProjectId = id(&project, "project")?;
    api.engine.delete_project(project, &caller.actor()).await?;
    Ok(StatusCode::NO_CONTENT)
}
