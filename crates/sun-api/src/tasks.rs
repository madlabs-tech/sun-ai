//! A task's lifecycle over HTTP: `PATCH /tasks/{id}` (fields and moves),
//! comments, the activity log (JSON or SSE), its runs and Stop
//! (`interrupt`). A key in any `/tasks/{id}/…` path (`ENG-12`, any case) is
//! the task's id by the time a handler runs ([`key_paths`]).
//! Writes need `run` (`require_by_method`);
//! [`crate::workspace_access::guard`] answers 404 for a hidden workspace's
//! task. The move rules are the engine's; an illegal move is 409 with a
//! plain sentence.

use axum::extract::{Path, Query, Request, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::{Extension, Json};
use serde::Deserialize;
use sun_core::model::{TaskActivity, TaskKey};
use sun_core::{AgentId, Error, RunStatus, TaskId, TaskStatus};
use sun_engine::{Actor, TaskChange};

use crate::Api;
use crate::auth::Caller;
use crate::error::{ApiError, ApiResult};
use crate::routes::{Body, summary};
use crate::wire::{self, TaskRunView, TaskView};
use crate::{routes, sse};

/// Before routing (one owner for keys in paths): a task key in
/// `/api/v1/tasks/{key}[/…]` becomes the task's ULID, so every task route,
/// the access guard and the handlers see an id. An unknown key becomes the
/// nil id, which every handler answers 404. Anything else is left alone.
pub(crate) async fn key_paths(
    State(api): State<Api>,
    mut request: Request,
    next: Next,
) -> Response {
    const PREFIX: &str = "/api/v1/tasks/";
    let uri = request.uri();
    let Some(rest) = uri.path().strip_prefix(PREFIX) else {
        return next.run(request).await;
    };
    let (segment, tail) = rest.split_at(rest.find('/').unwrap_or(rest.len()));
    let key = match segment.parse::<TaskKey>() {
        Ok(key) if segment.parse::<TaskId>().is_err() => key,
        _ => return next.run(request).await,
    };
    let id = match api.engine.task_id_by_key(&key).await {
        Ok(id) => id,
        Err(Error::NotFound(_)) => TaskId(ulid::Ulid::nil()),
        Err(e) => return ApiError::from(e).into_response(),
    };
    let query = uri.query().map(|q| format!("?{q}")).unwrap_or_default();
    let rewritten = format!("{PREFIX}{id}{tail}{query}");
    match rewritten.parse() {
        Ok(rewritten) => *request.uri_mut() = rewritten,
        Err(_) => return next.run(request).await,
    }
    next.run(request).await
}

/// Moves the task as `actor` dragged it; an illegal move is 409.
pub(crate) async fn move_to(
    api: &Api,
    task: TaskId,
    to: TaskStatus,
    actor: Actor,
) -> ApiResult<()> {
    api.engine
        .move_task_manually(task, to, Some(actor))
        .await
        .map_err(refused)
}

/// An illegal task move as one plain sentence (409); other errors as usual.
fn refused(error: Error) -> ApiError {
    let Error::InvalidTransition {
        machine: "TaskStatus",
        from,
        to,
    } = error
    else {
        return error.into();
    };
    let label = |status: &str| match status.parse::<TaskStatus>() {
        Ok(TaskStatus::Backlog) => "Backlog",
        Ok(TaskStatus::Todo) => "Todo",
        Ok(TaskStatus::InProgress) => "In progress",
        Ok(TaskStatus::Verifying) => "Verifying",
        Ok(TaskStatus::NeedsYou) => "Needs you",
        Ok(TaskStatus::Done) => "Done",
        Ok(TaskStatus::Blocked) => "Blocked",
        Ok(TaskStatus::Failed) => "Failed",
        Ok(TaskStatus::Cancelled) => "Cancelled",
        Err(_) => "another status",
    };
    let sentence = match (from, to) {
        ("done", _) => "This task is done; a done task doesn't move.".to_owned(),
        (_, "verifying") => {
            "Only the engine moves a task that runs a workflow or an agent to Verifying.".to_owned()
        }
        // Needs you → Done is an edge: refused only while its run goes on.
        ("needs_you", "done") => "Approve or cancel the run first.".to_owned(),
        (_, "done") => "A task moves to Done only from Needs you.".to_owned(),
        _ => format!("A task can't move from {} to {}.", label(from), label(to)),
    };
    ApiError::conflict(sentence)
}

/// `PATCH /tasks/{id}` with any of `{title, body, status, priority,
/// project_id, position}`; `project_id: null` takes it out of its project.
pub(crate) async fn update(
    State(api): State<Api>,
    Extension(caller): Extension<Caller>,
    Path(task): Path<String>,
    Body(change): Body<TaskChange>,
) -> ApiResult<Json<TaskView>> {
    if change
        .body
        .as_ref()
        .is_some_and(|b| b.len() > routes::MAX_BODY)
    {
        return Err(ApiError::invalid(
            "The task description is longer than 64 KB.",
        ));
    }
    let task = routes::id(&task, "task")?;
    api.engine
        .update_task(task, change, Some(caller.task_actor()))
        .await
        .map_err(refused)?;
    summary(&api, task).await
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct NewComment {
    body: String,
    #[serde(default)]
    mentions: Vec<AgentId>,
}

/// `POST /tasks/{id}/comments {body, mentions?}` (1–10 000 characters;
/// `mentions`: at most five active agents of the task's workspace, which
/// then wake instead of the assignee).
pub(crate) async fn comment(
    State(api): State<Api>,
    Extension(caller): Extension<Caller>,
    Path(task): Path<String>,
    Body(input): Body<NewComment>,
) -> ApiResult<(StatusCode, Json<TaskActivity>)> {
    let task = routes::id(&task, "task")?;
    let activity = api
        .engine
        .comment(
            task,
            &input.body,
            Some(caller.task_actor()),
            &input.mentions,
        )
        .await?;
    Ok((StatusCode::CREATED, Json(activity)))
}

#[derive(Deserialize)]
pub(crate) struct After {
    after: Option<i64>,
}

/// `GET /tasks/{id}/activity[?after=seq]`: the log, oldest first; with
/// `Accept: text/event-stream`, then live (SSE, `Last-Event-ID` resumes).
pub(crate) async fn activity(
    State(api): State<Api>,
    Path(task): Path<String>,
    Query(query): Query<After>,
    headers: HeaderMap,
) -> ApiResult<Response> {
    let task = routes::id(&task, "task")?;
    let after = headers
        .get("last-event-id")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse().ok())
        .or(query.after)
        .unwrap_or(0)
        .max(0);
    let wants_stream = headers
        .get(header::ACCEPT)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.contains("text/event-stream"));
    if wants_stream {
        let live = api.engine.subscribe_activity();
        let feed = sse::ActivityFeed {
            engine: api.engine.clone(),
            task_id: task,
        };
        return Ok(sse::stream(feed, live, after).into_response());
    }
    Ok(Json(api.engine.activity(task, after).await?).into_response())
}

/// `GET /tasks/{id}/runs`: attempt, trigger, status, cost, started and ended.
pub(crate) async fn runs(
    State(api): State<Api>,
    Path(task): Path<String>,
) -> ApiResult<Json<Vec<TaskRunView>>> {
    let task = routes::id(&task, "task")?;
    Ok(Json(wire::task_runs(&api.engine, task).await?))
}

/// `POST /tasks/{id}/interrupt` (Stop): pauses the running run's agents;
/// the task goes Blocked. 409 when nothing runs.
pub(crate) async fn interrupt(
    State(api): State<Api>,
    Extension(caller): Extension<Caller>,
    Path(task): Path<String>,
) -> ApiResult<Json<TaskView>> {
    let task = routes::id(&task, "task")?;
    let running = api.engine.runs_of_task(task).await?.pop();
    if running.is_none_or(|r| r.status != RunStatus::Running) {
        return Err(ApiError::conflict("Nothing is running on this task."));
    }
    api.engine
        .interrupt(task, Some(caller.task_actor()))
        .await
        .map_err(refused)?;
    summary(&api, task).await
}
