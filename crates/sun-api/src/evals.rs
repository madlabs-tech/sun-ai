//! Eval replay: `/api/v1/evals` and `/api/v1/eval_sets`.

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::{Extension, Json};
use serde::Deserialize;
use sun_core::TaskId;
use sun_core::model::Permission;
use sun_core::model::{EvalRun, EvalSet};
use sun_engine::{EvalSpec, OnWorkspace};

use crate::Api;
use crate::auth::Caller;
use crate::error::ApiResult;
use crate::routes::{Body, id};
use crate::workspace_access::{NamedWorkspaces, visible};

const PAGE: u32 = 50;
const MAX_PAGE: u32 = 200;

#[derive(Deserialize)]
pub(crate) struct Page {
    limit: Option<u32>,
}

/// `GET /evals?limit=`: newest first; only evals whose every task is on a
/// workspace the caller sees.
pub(crate) async fn list(
    State(api): State<Api>,
    Extension(caller): Extension<Caller>,
    Query(page): Query<Page>,
) -> ApiResult<Json<Vec<EvalRun>>> {
    let limit = page.limit.unwrap_or(PAGE).clamp(1, MAX_PAGE);
    let mut out = Vec::new();
    for eval in api.engine.eval_runs(limit).await? {
        if visible(&api, &caller, OnWorkspace::Eval(eval.id)).await? {
            out.push(eval);
        }
    }
    Ok(Json(out))
}

/// `POST /evals`: starts replaying; `202` with the `running` eval. A
/// candidate workflow with a `bypass_permissions` step needs what saving it
/// needs: `manage_runtimes` on the picked tasks' workspaces and
/// `runtimes.allow_bypass`, and the caller allowed by the owner's bypass policy.
pub(crate) async fn start(
    State(api): State<Api>,
    Extension(caller): Extension<Caller>,
    named: Option<Extension<NamedWorkspaces>>,
    Body(spec): Body<EvalSpec>,
) -> ApiResult<(StatusCode, Json<EvalRun>)> {
    if spec
        .candidate
        .workflow
        .as_deref()
        .is_some_and(sun_engine::sets_bypass)
    {
        let named = named.as_ref().map(|Extension(n)| n);
        crate::auth::allowed_in(&caller, named, &[Permission::ManageRuntimes])?;
        crate::hosts::check_bypass_allowed(&api, true)?;
        // It runs at once in a sandbox, as nobody the engine could cap.
        crate::bypass::check_can_bypass(&api, &caller).await?;
    }
    Ok((
        StatusCode::ACCEPTED,
        Json(api.engine.start_eval(spec).await?),
    ))
}

/// `GET /evals/{id}`: with its report once `done`.
pub(crate) async fn get(
    State(api): State<Api>,
    Path(eval): Path<String>,
) -> ApiResult<Json<EvalRun>> {
    let eval = id(&eval, "eval")?;
    Ok(Json(api.engine.eval_run(eval).await?))
}

/// `GET /eval_sets`: newest first; only sets whose every task the caller sees.
pub(crate) async fn sets(
    State(api): State<Api>,
    Extension(caller): Extension<Caller>,
) -> ApiResult<Json<Vec<EvalSet>>> {
    let mut out = Vec::new();
    for set in api.engine.eval_sets().await? {
        if visible(&api, &caller, OnWorkspace::EvalSet(set.id)).await? {
            out.push(set);
        }
    }
    Ok(Json(out))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct NewSet {
    name: String,
    task_ids: Vec<TaskId>,
}

/// `POST /eval_sets {name, task_ids}`.
pub(crate) async fn save_set(
    State(api): State<Api>,
    Body(input): Body<NewSet>,
) -> ApiResult<(StatusCode, Json<EvalSet>)> {
    let set = api
        .engine
        .save_eval_set(&input.name, input.task_ids)
        .await?;
    Ok((StatusCode::CREATED, Json(set)))
}
