//! Long-term memory: `/api/v1/memory`,
//! `/memory/{id}/review`, `/memory/metrics`, and the stars that propose it:
//! `/corrections/{id}/star`, `/notes/{id}/star`.

use axum::Json;
use axum::extract::{Extension, Path, State};
use serde::{Deserialize, Serialize};
use sun_core::model::{MemoryItem, MemoryStatus};
use sun_engine::{MemoryMetrics, MemoryView};

use crate::Api;
use crate::auth::Caller;
use crate::error::{ApiError, ApiResult};
use crate::routes::{Body, id};

pub(crate) async fn list(State(api): State<Api>) -> ApiResult<Json<Vec<MemoryView>>> {
    Ok(Json(api.engine.memory().await?))
}

pub(crate) async fn metrics(State(api): State<Api>) -> ApiResult<Json<MemoryMetrics>> {
    Ok(Json(api.engine.memory_metrics().await?))
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
