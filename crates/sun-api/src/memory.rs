//! Long-term memory, per workspace: `/api/v1/memory?workspace=`,
//! `/memory/{id}/review`, `/memory/metrics?workspace=`, and the stars that
//! propose it: `/corrections/{id}/star`, `/notes/{id}/star`. Any member
//! reads; reviewing and starring need `manage_settings` in the item's
//! workspace (the router). Only a deployment owner may leave out
//! `?workspace=` (every workspace's).

use axum::Json;
use axum::extract::{Extension, Path, Query, State};
use serde::{Deserialize, Serialize};
use sun_core::model::{MemoryItem, MemoryStatus};
use sun_engine::{MemoryMetrics, MemoryView};

use crate::Api;
use crate::auth::Caller;
use crate::error::{ApiError, ApiResult};
use crate::routes::{Body, id};
use crate::workspace_access::{InWorkspace, listed_workspace};

pub(crate) async fn list(
    State(api): State<Api>,
    Extension(caller): Extension<Caller>,
    Query(query): Query<InWorkspace>,
) -> ApiResult<Json<Vec<MemoryView>>> {
    let workspace = listed_workspace(&caller, query.workspace.as_deref())?;
    Ok(Json(api.engine.memory(workspace).await?))
}

pub(crate) async fn metrics(
    State(api): State<Api>,
    Extension(caller): Extension<Caller>,
    Query(query): Query<InWorkspace>,
) -> ApiResult<Json<MemoryMetrics>> {
    let workspace = listed_workspace(&caller, query.workspace.as_deref())?;
    Ok(Json(api.engine.memory_metrics(workspace).await?))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Review {
    status: String,
}

/// Approve (`active`), reject or retire (`retired`); 409 for any other move.
pub(crate) async fn review(
    State(api): State<Api>,
    Extension(by): Extension<Caller>,
    Path(item): Path<String>,
    Body(input): Body<Review>,
) -> ApiResult<Json<MemoryItem>> {
    let to = match input.status.parse() {
        Ok(to @ (MemoryStatus::Active | MemoryStatus::Retired)) => to,
        _ => {
            return Err(ApiError::invalid(
                "Send {\"status\": \"active\"} or \"retired\".",
            ));
        }
    };
    let item = id(&item, "memory item")?;
    let by = format!("user:{}", by.principal.user);
    Ok(Json(api.engine.review_memory(item, to, &by).await?))
}

/// Whether a star made a new proposal (`false`: starred before).
#[derive(Serialize)]
pub(crate) struct Starred {
    created: bool,
}

pub(crate) async fn star_correction(
    State(api): State<Api>,
    Extension(by): Extension<Caller>,
    Path(correction): Path<String>,
) -> ApiResult<Json<Starred>> {
    let correction = id(&correction, "correction")?;
    let by = format!("user:{}", by.principal.user);
    let created = api.engine.star_correction(correction, &by).await?;
    Ok(Json(Starred { created }))
}

pub(crate) async fn star_note(
    State(api): State<Api>,
    Extension(by): Extension<Caller>,
    Path(note): Path<String>,
) -> ApiResult<Json<Starred>> {
    let note = id(&note, "note")?;
    let by = format!("user:{}", by.principal.user);
    let created = api.engine.star_note(note, &by).await?;
    Ok(Json(Starred { created }))
}
