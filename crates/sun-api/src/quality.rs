//! Data touched and post-merge quality:
//! `/api/v1/tasks/{id}/quality`, `/tasks/{id}/quality/check`, `/quality/metrics`.

use axum::Json;
use axum::extract::{Path, State};
use serde::Serialize;
use sun_core::TaskId;
use sun_core::model::TaskQuality;
use sun_engine::{QualityMetrics, Touch};

use crate::Api;
use crate::error::ApiResult;
use crate::routes::id;

/// A task's data-touched badge and quality record.
#[derive(Serialize)]
pub(crate) struct TaskQualityView {
    /// Latest detection of the latest run (best effort, "detected").
    data_touched: Vec<Touch>,
    /// `None` until the PR merged.
    quality: Option<TaskQuality>,
    /// [`TaskQuality::held_up`].
    held_up: Option<bool>,
}

pub(crate) async fn get(
    State(api): State<Api>,
    Path(task): Path<String>,
) -> ApiResult<Json<TaskQualityView>> {
    let task_id: TaskId = id(&task, "task")?;
    let quality = api.engine.quality(task_id).await?;
    Ok(Json(TaskQualityView {
        data_touched: api.engine.data_touched(task_id).await?,
        held_up: quality.as_ref().and_then(TaskQuality::held_up),
        quality,
    }))
}

/// Re-checks the follow-up signals now; 422 before the PR merged.
pub(crate) async fn check(
    State(api): State<Api>,
    Path(task): Path<String>,
) -> ApiResult<Json<TaskQuality>> {
    Ok(Json(api.engine.check_quality(id(&task, "task")?).await?))
}

pub(crate) async fn metrics(State(api): State<Api>) -> ApiResult<Json<QualityMetrics>> {
    Ok(Json(api.engine.quality_metrics().await?))
}
