//! Design-canvas previews: start a task's dev server on its host and
//! proxy it at `/preview/<token>/…` through the host WebSocket.
//!
//! **Design:** the dev server binds the host's loopback only; every request
//! travels kernel → host as a `preview.http` frame (no extra port, works for
//! remote hosts). The token in the path is the credential (256 random bits,
//! kernel memory, one per task and side), so the iframe's subresources load
//! without the Sun cookie. Every preview response carries
//! `Content-Security-Policy: sandbox …` without `allow-same-origin`: the
//! agent-written app runs in an opaque origin and cannot call `/api/v1` with
//! the user's session, even when opened top-level. `Cookie`/`Authorization`
//! never reach the dev server and its `Set-Cookie` never reaches the browser.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use axum::Router;
use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, HeaderName, HeaderValue, Method, StatusCode, Uri, header};
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::any;
use serde::Deserialize;
use serde_json::{Value, json};
use sun_core::host_protocol::{HttpRequest, KernelFrame, PreviewRequest, PreviewSide, ReplyBody};
use sun_core::{Error, HostId, TaskId};

use crate::Api;
use crate::error::{ApiError, ApiResult};
use crate::hosts::{random_hex, unexpected};
use crate::routes::{Body, id};

/// Seconds the host waits for the dev server to accept connections.
const START_S: u64 = 300;
/// Extra wait for the host to clone/fetch the repo before starting it.
const PREPARE: Duration = Duration::from_secs(15 * 60);
/// Reply wait for one tunnelled request (the host gives up after 60 s).
const HTTP_WAIT: Duration = Duration::from_secs(75);
/// Request bodies forwarded to a preview at most.
const MAX_BODY: usize = 1024 * 1024;
/// The preview runs in an opaque origin: scripts, forms, popups and modals
/// work; same-origin access, top navigation and downloads do not.
const SANDBOX: &str = "sandbox allow-scripts allow-forms allow-popups allow-modals";
/// Request headers that carry the user's Sun credentials.
const PRIVATE: &[&str] = &["cookie", "authorization"];
/// Response headers that must not reach the browser.
const DROPPED: &[&str] = &["set-cookie", "x-frame-options", "content-length"];
/// Appended to every uncompressed HTML answer: the canvas bridge (pins),
/// served by the web UI at `/preview_bridge.js` (`web/public`). After
/// `</html>` the parser still runs it, and the doctype stays first.
const BRIDGE: &[u8] = b"<script src=\"/preview_bridge.js\"></script>";

/// Live preview tokens → (task, side). Kernel memory: a restart forgets them
/// and the canvas starts the previews again (the host restarts a server
/// whose base changed).
#[derive(Default, Clone)]
pub(crate) struct Previews(Arc<Mutex<HashMap<String, (TaskId, PreviewSide)>>>);

impl Previews {
    fn lock(&self) -> MutexGuard<'_, HashMap<String, (TaskId, PreviewSide)>> {
        self.0.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn token_of(&self, task: TaskId, side: PreviewSide) -> Option<String> {
        self.lock()
            .iter()
            .find(|(_, v)| **v == (task, side))
            .map(|(k, _)| k.clone())
    }
}

fn view(side: PreviewSide, token: &str) -> Value {
    json!({ "side": side, "url": format!("/preview/{token}/") })
}

fn host(api: &Api) -> ApiResult<HostId> {
    api.preview_host.ok_or_else(|| {
        Error::Unavailable("This kernel has no runtime host for previews.".into()).into()
    })
}

/// `GET /api/v1/tasks/{id}/previews`: the task's running previews.
pub(crate) async fn list(
    State(api): State<Api>,
    Path(task): Path<String>,
) -> ApiResult<axum::Json<Vec<Value>>> {
    let task_id: TaskId = id(&task, "task")?;
    Ok(axum::Json(
        PreviewSide::ALL
            .iter()
            .filter_map(|side| {
                api.previews
                    .token_of(task_id, *side)
                    .map(|t| view(*side, &t))
            })
            .collect(),
    ))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Start {
    side: PreviewSide,
}

/// `POST /api/v1/tasks/{id}/previews {side}`: starts (or reuses) the dev
/// server of one side and answers once it listens.
pub(crate) async fn start(
    State(api): State<Api>,
    Path(task): Path<String>,
    Body(input): Body<Start>,
) -> ApiResult<axum::Json<Value>> {
    let task_id: TaskId = id(&task, "task")?;
    let host = host(&api)?;
    let worktree = api.engine.worktree(task_id).await?;
    let token = match api.previews.token_of(task_id, input.side) {
        Some(token) => token,
        None => random_hex()?,
    };
    let request = PreviewRequest {
        worktree,
        side: input.side,
        base_path: format!("/preview/{token}/"),
        timeout_s: START_S,
    };
    let wait = PREPARE + Duration::from_secs(START_S);
    match api
        .hosts
        .request(host, wait, |id| KernelFrame::PreviewStart { id, request })
        .await?
    {
        ReplyBody::Done => {}
        other => return Err(unexpected(other).into()),
    }
    api.previews
        .lock()
        .insert(token.clone(), (task_id, input.side));
    Ok(axum::Json(view(input.side, &token)))
}

/// `DELETE /api/v1/tasks/{id}/previews`: stops both dev servers.
pub(crate) async fn stop(
    State(api): State<Api>,
    Path(task): Path<String>,
) -> ApiResult<StatusCode> {
    let task_id: TaskId = id(&task, "task")?;
    let host = host(&api)?;
    api.previews.lock().retain(|_, (t, _)| *t != task_id);
    match api
        .hosts
        .request(host, crate::hosts::REPLY, |id| KernelFrame::PreviewStop {
            id,
            task_id,
        })
        .await?
    {
        ReplyBody::Done => Ok(StatusCode::NO_CONTENT),
        other => Err(unexpected(other).into()),
    }
}

/// `/preview/<token>` (no auth: the token is the credential), every method.
pub(crate) fn router(api: Api) -> Router {
    Router::new()
        .route("/preview/{token}", any(add_slash))
        .route("/preview/{token}/", any(proxy))
        .route("/preview/{token}/{*rest}", any(proxy))
        .with_state(api)
}

async fn add_slash(Path(token): Path<String>) -> Redirect {
    Redirect::permanent(&format!("/preview/{token}/"))
}

async fn proxy(
    State(api): State<Api>,
    Path(params): Path<HashMap<String, String>>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let token = params.get("token").map(String::as_str).unwrap_or_default();
    let Some((task_id, side)) = api.previews.lock().get(token).copied() else {
        return (
            StatusCode::NOT_FOUND,
            "This preview has stopped. Start it again from the canvas.",
        )
            .into_response();
    };
    if body.len() > MAX_BODY {
        return ApiError::invalid("Preview requests are limited to 1 MiB.").into_response();
    }
    let Ok(host) = host(&api) else {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    };
    let request = HttpRequest {
        method: method.to_string(),
        path: uri.path_and_query().map_or("/", |p| p.as_str()).to_owned(),
        headers: headers
            .iter()
            .filter(|(name, _)| !PRIVATE.contains(&name.as_str()))
            .filter_map(|(name, value)| {
                Some((name.as_str().to_owned(), value.to_str().ok()?.to_owned()))
            })
            .collect(),
        body: body.to_vec(),
    };
    let reply = api
        .hosts
        .request(host, HTTP_WAIT, |id| KernelFrame::PreviewHttp {
            id,
            task_id,
            side,
            request,
        })
        .await;
    let answer = match reply {
        Ok(ReplyBody::Http { response }) => response,
        Ok(other) => return ApiError::from(unexpected(other)).into_response(),
        Err(e) => return ApiError::from(e).into_response(),
    };
    let mut body = answer.body;
    if is_plain_html(&answer.headers) {
        body.extend_from_slice(BRIDGE);
    }
    let mut response = Response::new(axum::body::Body::from(body));
    *response.status_mut() = StatusCode::from_u16(answer.status).unwrap_or(StatusCode::BAD_GATEWAY);
    let out = response.headers_mut();
    for (name, value) in answer.headers {
        let (Ok(name), Ok(value)) = (
            HeaderName::from_bytes(name.as_bytes()),
            HeaderValue::from_str(&value),
        ) else {
            continue;
        };
        if !DROPPED.contains(&name.as_str()) {
            out.append(name, value);
        }
    }
    out.append(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(SANDBOX),
    );
    // Opaque-origin module scripts are CORS fetches; the token guards reads.
    out.insert(
        header::ACCESS_CONTROL_ALLOW_ORIGIN,
        HeaderValue::from_static("*"),
    );
    out.insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("no-referrer"),
    );
    response
}

/// `text/html` without a `Content-Encoding` (a compressed body can't be appended to).
fn is_plain_html(headers: &[(String, String)]) -> bool {
    let header = |name: &str| headers.iter().find(|(n, _)| n.eq_ignore_ascii_case(name));
    header("content-encoding").is_none()
        && header("content-type")
            .is_some_and(|(_, v)| v.trim_start().to_ascii_lowercase().starts_with("text/html"))
}
