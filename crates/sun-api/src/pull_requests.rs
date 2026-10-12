//! A task's linked pull requests (the task page's PR sidebar):
//!
//! - `GET /tasks/{id}/pull_requests` → `{pull_requests, auto_complete}`;
//! - `POST /tasks/{id}/pull_requests {url}` or `{pull_request_id}`: links a
//!   PR the workspace already mirrors (404 until its delivery arrived);
//! - `DELETE /tasks/{id}/pull_requests/{pr_id}`: removes it, and remembers
//!   not to link it again automatically;
//! - `PUT /tasks/{id}/pr_auto_complete {disabled}`: the per-task switch of
//!   "move when PRs merge".
//!
//! All answer the same shape. Reads are for whoever sees the task; writes
//! need `run` (`require_by_method`, the task-write permission) and a member
//! (the link is recorded as theirs: the CLI token has no member).

use axum::extract::{Path, State};
use axum::{Extension, Json};
use serde::Deserialize;
use sun_core::{MemberId, PullRequestId};
use sun_engine::TaskPullRequests;

use crate::Api;
use crate::auth::Caller;
use crate::error::{ApiError, ApiResult};
use crate::routes::{Body, id};

/// Longest URL a person may paste.
const MAX_URL: usize = 2048;

/// The caller as a member, or 403: a link names who made it.
fn member(caller: &Caller) -> ApiResult<MemberId> {
    caller.member_id.ok_or_else(|| {
        ApiError::new(
            axum::http::StatusCode::FORBIDDEN,
            "forbidden",
            "Sign in as a member to change a task's pull requests.",
        )
    })
}

/// `GET /tasks/{id}/pull_requests`.
pub(crate) async fn list(
    State(api): State<Api>,
    Path(task): Path<String>,
) -> ApiResult<Json<TaskPullRequests>> {
    let task = id(&task, "task")?;
    Ok(Json(api.engine.task_pull_requests(task).await?))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Link {
    /// The PR's page, as pasted.
    url: Option<String>,
    /// A mirrored PR's id (undo of a removal).
    pull_request_id: Option<String>,
}

/// `POST /tasks/{id}/pull_requests {url} | {pull_request_id}`.
pub(crate) async fn link(
    State(api): State<Api>,
    Extension(caller): Extension<Caller>,
    Path(task): Path<String>,
    Body(input): Body<Link>,
) -> ApiResult<Json<TaskPullRequests>> {
    let task = id(&task, "task")?;
    let by = member(&caller)?;
    let url = input
        .url
        .map(|u| u.trim().to_owned())
        .filter(|u| !u.is_empty());
    if url.as_ref().is_some_and(|u| u.len() > MAX_URL) {
        return Err(ApiError::invalid(
            "Paste the URL of a pull request or merge request.",
        ));
    }
    let pull_request = input
        .pull_request_id
        .as_deref()
        .map(|p| id::<PullRequestId>(p, "pull request"))
        .transpose()?;
    if url.is_none() && pull_request.is_none() {
        return Err(ApiError::invalid(
            "Paste the URL of a pull request or merge request.",
        ));
    }
    Ok(Json(
        api.engine
            .link_pull_request(task, by, url, pull_request)
            .await?,
    ))
}

/// `DELETE /tasks/{id}/pull_requests/{pr_id}`.
pub(crate) async fn unlink(
    State(api): State<Api>,
    Extension(caller): Extension<Caller>,
    Path((task, pull_request)): Path<(String, String)>,
) -> ApiResult<Json<TaskPullRequests>> {
    let task = id(&task, "task")?;
    let pull_request = id(&pull_request, "pull request")?;
    let by = member(&caller)?;
    Ok(Json(
        api.engine
            .unlink_pull_request(task, pull_request, by)
            .await?,
    ))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct AutoCompleteSwitch {
    disabled: bool,
}

/// `PUT /tasks/{id}/pr_auto_complete {disabled}`.
pub(crate) async fn set_auto_complete(
    State(api): State<Api>,
    Extension(caller): Extension<Caller>,
    Path(task): Path<String>,
    Body(input): Body<AutoCompleteSwitch>,
) -> ApiResult<Json<TaskPullRequests>> {
    let task = id(&task, "task")?;
    let by = member(&caller)?;
    Ok(Json(
        api.engine
            .set_pr_auto_complete(task, input.disabled, by)
            .await?,
    ))
}
