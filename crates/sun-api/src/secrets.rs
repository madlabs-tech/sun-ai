//! Secrets, admin only: list metadata, set, delete. No route returns
//! a value, and a request body is never logged or echoed.

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
}

/// An ecosystem or mod scope. Workspace secrets (git connection tokens)
/// are the kernel's own: set and deleted only through `/vcs`.
fn scope(kind: &str, id: &str) -> ApiResult<SecretScope> {
    match SecretScope::from_parts(kind, id) {
        Ok(SecretScope::Workspace(_)) | Err(_) => Err(ApiError::invalid(
            "Scope is `ecosystem/<id>` or `mod/<mod name>`.",
        )),
        Ok(scope) => Ok(scope),
    }
}

/// `GET /secrets[?scope_kind=&scope_id=]`: names and times, never values.
pub(crate) async fn list(
    State(api): State<Api>,
    Query(filter): Query<ScopeFilter>,
) -> ApiResult<Json<Vec<Secret>>> {
    let scope = match (filter.scope_kind, filter.scope_id) {
        (Some(kind), Some(id)) => Some(scope(&kind, &id)?),
        (None, None) => None,
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

/// `PUT /secrets/{kind}/{id}/{name} {value}`: seals and stores it.
pub(crate) async fn put(
    State(api): State<Api>,
    caller: axum::Extension<Caller>,
    Path((kind, id, name)): Path<(String, String, String)>,
    Body(input): Body<Value>,
) -> ApiResult<Json<Secret>> {
    let scope = scope(&kind, &id)?;
    let secret = api
        .engine
        .set_secret(&scope, &name, &input.value, &caller.actor())
        .await?;
    Ok(Json(secret))
}

/// `DELETE /secrets/{kind}/{id}/{name}`: 204, or 404 when there is none.
pub(crate) async fn remove(
    State(api): State<Api>,
    caller: axum::Extension<Caller>,
    Path((kind, id, name)): Path<(String, String, String)>,
) -> ApiResult<StatusCode> {
    let scope = scope(&kind, &id)?;
    match api
        .engine
        .delete_secret(&scope, &name, &caller.actor())
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
