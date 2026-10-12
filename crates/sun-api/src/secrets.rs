//! Secrets: list metadata, set, delete. No route returns a value, and a
//! request body is never logged or echoed.
//!
//! - A workspace's (`GET /secrets?workspace=`, `PUT|DELETE
//!   /secrets/workspace/{id}/{name}`): `manage_secrets` there (the router).
//!   The git connection tokens the kernel keeps there
//!   ([`Secret::is_connection`]) are never listed, set or deleted here.
//! - The deployment's (`ecosystem` and `mod` scopes): deployment owners. The
//!   ecosystem scope holds only the deployment's own names
//!   ([`Secret::is_deployment`]); everything else is a workspace's.

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use serde::Deserialize;
use sun_core::model::{Secret, SecretScope};

use crate::Api;
use crate::auth::Caller;
use crate::error::{ApiError, ApiResult};
use crate::routes::Body;

#[derive(Deserialize)]
pub(crate) struct ScopeFilter {
    scope_kind: Option<String>,
    scope_id: Option<String>,
    workspace: Option<String>,
}

/// An ecosystem or mod scope. Workspace secrets have their own routes.
fn scope(kind: &str, id: &str) -> ApiResult<SecretScope> {
    match SecretScope::from_parts(kind, id) {
        Ok(SecretScope::Workspace(_)) | Err(_) => Err(ApiError::invalid(
            "Scope is `ecosystem/<id>` or `mod/<mod name>`.",
        )),
        Ok(scope) => Ok(scope),
    }
}

/// Workspace `id`'s scope, for a name that isn't a git connection's.
fn workspace_scope(id: &str, name: &str) -> ApiResult<SecretScope> {
    if Secret::is_connection(name) {
        return Err(ApiError::invalid(format!(
            "`{name}` belongs to a git connection; manage it under the workspace's Code settings."
        )));
    }
    let id = id
        .parse()
        .map_err(|_| ApiError::invalid("`workspace` is not a workspace id."))?;
    Ok(SecretScope::Workspace(id))
}

/// `GET /secrets?workspace=` (a workspace's, but its git connections') or,
/// for deployment owners, `GET /secrets[?scope_kind=&scope_id=]` (the
/// ecosystem's and mods'): names and times, never values.
pub(crate) async fn list(
    State(api): State<Api>,
    Query(filter): Query<ScopeFilter>,
) -> ApiResult<Json<Vec<Secret>>> {
    let scope = match (filter.workspace, filter.scope_kind, filter.scope_id) {
        (Some(workspace), None, None) => {
            let scope = workspace_scope(&workspace, "")?;
            let secrets = api.engine.secrets(Some(&scope)).await?;
            let mine = secrets
                .into_iter()
                .filter(|s| !Secret::is_connection(&s.name));
            return Ok(Json(mine.collect()));
        }
        (Some(_), _, _) => {
            return Err(ApiError::invalid(
                "Pass either workspace, or scope_kind and scope_id.",
            ));
        }
        (None, Some(kind), Some(id)) => Some(scope(&kind, &id)?),
        (None, None, None) => None,
        _ => return Err(ApiError::invalid("Pass both scope_kind and scope_id.")),
    };
    let secrets = api.engine.secrets(scope.as_ref()).await?;
    Ok(Json(
        secrets
            .into_iter()
            .filter(|s| !matches!(s.scope, SecretScope::Workspace(_)))
            .collect(),
    ))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Value {
    value: String,
}

/// `PUT /secrets/{kind}/{id}/{name} {value}`: seals and stores it. The
/// ecosystem scope takes only a deployment name.
pub(crate) async fn put(
    State(api): State<Api>,
    caller: axum::Extension<Caller>,
    Path((kind, id, name)): Path<(String, String, String)>,
    Body(input): Body<Value>,
) -> ApiResult<Json<Secret>> {
    let scope = scope(&kind, &id)?;
    if matches!(scope, SecretScope::Ecosystem(_)) && !Secret::is_deployment(&name) {
        return Err(ApiError::invalid(format!(
            "`{name}` is a workspace secret: PUT /secrets/workspace/<workspace id>/{name}."
        )));
    }
    set(&api, &caller, &scope, &name, &input.value).await
}

/// `PUT /secrets/workspace/{id}/{name} {value}`: seals and stores it.
pub(crate) async fn put_in_workspace(
    State(api): State<Api>,
    caller: axum::Extension<Caller>,
    Path((workspace, name)): Path<(String, String)>,
    Body(input): Body<Value>,
) -> ApiResult<Json<Secret>> {
    let scope = workspace_scope(&workspace, &name)?;
    set(&api, &caller, &scope, &name, &input.value).await
}

async fn set(
    api: &Api,
    caller: &Caller,
    scope: &SecretScope,
    name: &str,
    value: &str,
) -> ApiResult<Json<Secret>> {
    let secret = api
        .engine
        .set_secret(scope, name, value, &caller.actor())
        .await?;
    Ok(Json(secret))
}

/// `DELETE /secrets/{kind}/{id}/{name}`: 204, or 404 when there is none.
pub(crate) async fn remove(
    State(api): State<Api>,
    caller: axum::Extension<Caller>,
    Path((kind, id, name)): Path<(String, String, String)>,
) -> ApiResult<StatusCode> {
    delete(&api, &caller, &scope(&kind, &id)?, &name).await
}

/// `DELETE /secrets/workspace/{id}/{name}`: 204, or 404 when there is none.
pub(crate) async fn remove_in_workspace(
    State(api): State<Api>,
    caller: axum::Extension<Caller>,
    Path((workspace, name)): Path<(String, String)>,
) -> ApiResult<StatusCode> {
    delete(&api, &caller, &workspace_scope(&workspace, &name)?, &name).await
}

async fn delete(
    api: &Api,
    caller: &Caller,
    scope: &SecretScope,
    name: &str,
) -> ApiResult<StatusCode> {
    match api
        .engine
        .delete_secret(scope, name, &caller.actor())
        .await?
    {
        true => Ok(StatusCode::NO_CONTENT),
        false => Err(ApiError::new(
            StatusCode::NOT_FOUND,
            "not_found",
            format!("There is no secret `{name}` here."),
        )),
    }
}
