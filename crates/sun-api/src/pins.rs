//! Design-canvas pins and evidence:
//! `/api/v1/tasks/{id}/pins`, their crops and commits, and `/tasks/{id}/screens`.
//! The target comes from the preview bridge, i.e. from agent-written code in
//! the browser, so every field is bounded here before it is stored.

use axum::extract::{Path, State};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::{Extension, Json};
use serde::Deserialize;
use sun_core::host_protocol::PreviewSide;
use sun_core::model::{Pin, PinEdit, PinStatus, PinTarget};
use sun_core::{PinId, TaskId};
use sun_engine::{Author, Design, PinChange, PinCommit, Screen};

use crate::Api;
use crate::auth::Caller;
use crate::error::{ApiError, ApiResult};
use crate::routes::{Body, id};

const MAX_SELECTOR: usize = 1000;
const MAX_TEXT: usize = 500;
const MAX_VALUE: usize = 500;
const MAX_STYLES: usize = 64;
const MAX_EDITS: usize = 32;
const MAX_COMMENT: usize = 4000;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct NewPin {
    target: PinTarget,
    #[serde(default)]
    comment: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Change {
    status: Option<PinStatus>,
    comment: Option<String>,
    edits: Option<Vec<PinEdit>>,
}

fn too_long(text: &str, max: usize) -> bool {
    text.len() > max
}

fn check_target(t: &PinTarget) -> ApiResult<()> {
    let rect = [t.rect.x, t.rect.y, t.rect.width, t.rect.height];
    let ok = !t.selector.trim().is_empty()
        && !too_long(&t.selector, MAX_SELECTOR)
        && !t.tag.is_empty()
        && t.tag.len() <= 64
        && !too_long(&t.text, MAX_TEXT)
        && (1..=10_000).contains(&t.width)
        && rect.iter().all(|v| v.is_finite() && v.abs() <= 1e6)
        && t.styles.len() <= MAX_STYLES
        && t.styles
            .iter()
            .all(|(k, v)| k.len() <= 64 && !too_long(v, MAX_VALUE))
        && t.source
            .as_ref()
            .is_none_or(|s| !s.file.is_empty() && !too_long(&s.file, MAX_SELECTOR))
        && (t.path.is_empty() || page(&t.path));
    if ok {
        Ok(())
    } else {
        Err(ApiError::invalid(
            "The pinned element is too large to store. Pin a smaller element.",
        ))
    }
}

/// A page path under the preview base: `/…`, bounded, no spaces or controls.
fn page(path: &str) -> bool {
    path.starts_with('/')
        && !too_long(path, MAX_SELECTOR)
        && !path.chars().any(|c| c.is_whitespace() || c.is_control())
}

fn check_comment(comment: &str) -> ApiResult<()> {
    if too_long(comment, MAX_COMMENT) {
        return Err(ApiError::invalid("Keep the request under 4000 characters."));
    }
    Ok(())
}

fn check_edits(edits: &[PinEdit]) -> ApiResult<()> {
    let ok = edits.len() <= MAX_EDITS
        && edits.iter().all(|e| {
            !e.property.is_empty()
                && e.property.len() <= 64
                && !too_long(&e.from, MAX_VALUE)
                && !too_long(&e.to, MAX_VALUE)
        });
    if ok {
        Ok(())
    } else {
        Err(ApiError::invalid(
            "Edits are limited to 32 values of up to 500 characters.",
        ))
    }
}

/// `GET /api/v1/tasks/{id}/pins`: the task's pins, oldest first.
pub(crate) async fn list(
    State(api): State<Api>,
    Path(task): Path<String>,
) -> ApiResult<Json<Vec<Pin>>> {
    let task_id: TaskId = id(&task, "task")?;
    api.engine.task(task_id).await?;
    Ok(Json(api.engine.pins(task_id).await?))
}

/// `POST /api/v1/tasks/{id}/pins {target, comment?}`: pins an element.
pub(crate) async fn create(
    State(api): State<Api>,
    Path(task): Path<String>,
    Body(input): Body<NewPin>,
) -> ApiResult<Json<Pin>> {
    let task_id: TaskId = id(&task, "task")?;
    check_target(&input.target)?;
    check_comment(&input.comment)?;
    let pin = api
        .engine
        .create_pin(task_id, input.target, input.comment.trim().to_owned())
        .await?;
    Ok(Json(pin))
}

/// `PATCH /api/v1/tasks/{id}/pins/{pin} {status?, comment?, edits?}`.
pub(crate) async fn update(
    State(api): State<Api>,
    Path((task, pin)): Path<(String, String)>,
    Body(input): Body<Change>,
) -> ApiResult<Json<Pin>> {
    let task_id: TaskId = id(&task, "task")?;
    let pin_id: PinId = id(&pin, "pin")?;
    if let Some(comment) = &input.comment {
        check_comment(comment)?;
    }
    if let Some(edits) = &input.edits {
        check_edits(edits)?;
    }
    let change = PinChange {
        status: input.status,
        comment: input.comment.map(|c| c.trim().to_owned()),
        edits: input.edits,
    };
    Ok(Json(api.engine.update_pin(task_id, pin_id, change).await?))
}

/// `DELETE /api/v1/tasks/{id}/pins/{pin}`.
pub(crate) async fn remove(
    State(api): State<Api>,
    Path((task, pin)): Path<(String, String)>,
) -> ApiResult<StatusCode> {
    let task_id: TaskId = id(&task, "task")?;
    let pin_id: PinId = id(&pin, "pin")?;
    api.engine.delete_pin(task_id, pin_id).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// `POST /api/v1/tasks/{id}/pins/{pin}/commit`: "Change it myself".
/// Commits the pin's edits to the task branch under the caller's name and
/// answers `{sha, pin}`; `422` when the edit doesn't map exactly to the
/// pin's source line (hand it to the agent).
pub(crate) async fn commit(
    State(api): State<Api>,
    Extension(caller): Extension<Caller>,
    Path((task, pin)): Path<(String, String)>,
) -> ApiResult<Json<PinCommit>> {
    let task_id: TaskId = id(&task, "task")?;
    let pin_id: PinId = id(&pin, "pin")?;
    let name = caller.principal.user.clone();
    let author = Author {
        email: caller
            .email()
            .map_or_else(|| format!("{name}@users.noreply.sun"), str::to_owned),
        name,
    };
    Ok(Json(
        api.engine.commit_pin_edit(task_id, pin_id, author).await?,
    ))
}

/// `GET /api/v1/tasks/{id}/pins/{pin}/crops/{before|after}`: the PNG crop.
pub(crate) async fn crop(
    State(api): State<Api>,
    Path((task, pin, side)): Path<(String, String, String)>,
) -> ApiResult<Response> {
    let task_id: TaskId = id(&task, "task")?;
    let pin_id: PinId = id(&pin, "pin")?;
    let side: PreviewSide = side
        .parse()
        .map_err(|_| ApiError::invalid("The crop is `before` or `after`."))?;
    let png = api.engine.pin_crop(task_id, pin_id, side).await?;
    Ok((
        [
            (header::CONTENT_TYPE, "image/png"),
            (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
            (header::CACHE_CONTROL, "private, no-cache"),
        ],
        png,
    )
        .into_response())
}

/// Screens a reviewer agreed on.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Screens {
    /// Page paths; default `/`.
    #[serde(default)]
    paths: Vec<String>,
    /// Breakpoints in CSS pixels.
    widths: Vec<u32>,
    /// Git host plugin for the PR; default `github`.
    #[serde(default, rename = "mod")]
    git_host: Option<String>,
}

/// `POST /api/v1/tasks/{id}/screens {paths?, widths, mod?}`: screenshots of
/// the agreed screens (before and after) on the task's timeline and PR.
pub(crate) async fn attach_screens(
    State(api): State<Api>,
    Path(task): Path<String>,
    Body(input): Body<Screens>,
) -> ApiResult<Json<Vec<Screen>>> {
    let task_id: TaskId = id(&task, "task")?;
    let paths = match input.paths {
        paths if paths.is_empty() => vec!["/".to_owned()],
        paths => paths,
    };
    let sizes_ok = (1..=3).contains(&input.widths.len())
        && input.widths.iter().all(|w| (100..=4000).contains(w));
    if paths.len() > 12 || !paths.iter().all(|p| page(p)) || !sizes_ok {
        return Err(ApiError::invalid(
            "Pick up to 12 page paths starting with / and 1 to 3 widths (100–4000 px).",
        ));
    }
    let git_host = input.git_host;
    if git_host.as_ref().is_some_and(|m| {
        m.is_empty()
            || !m
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    }) {
        return Err(ApiError::invalid("`mod` is a plugin name."));
    }
    let screens = api
        .engine
        .attach_screens(task_id, paths, input.widths, git_host)
        .await?;
    Ok(Json(screens))
}

/// `GET /api/v1/tasks/{id}/design`: the canvas screens from the repo's
/// `.sun-preview` and its design files (`DESIGN.md`, tokens) on the task
/// branch.
pub(crate) async fn design(
    State(api): State<Api>,
    Path(task): Path<String>,
) -> ApiResult<Json<Design>> {
    let task_id: TaskId = id(&task, "task")?;
    Ok(Json(api.engine.design(task_id).await?))
}
