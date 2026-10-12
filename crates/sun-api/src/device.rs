//! Live device view: `GET /api/v1/tasks/{id}/device` upgrades
//! to a WebSocket that shows the screen of the device the task's verify is
//! running on and forwards taps, text and keys to it.
//!
//! **Design:** the kernel pulls from the host socket and relays each piece as
//! a binary message, so a closed browser stops the capture and the host
//! keeps no subscription to the kernel. With `?video=h264` (the browser has
//! WebCodecs) it long-polls `device.video`: one H.264 access unit per binary
//! message, `mime` `video/h264`. Without it, or when the host can't stream,
//! it pulls one picture at a time (`device.screen`, a few fps). Text
//! messages carry the state as JSON tagged on `kind`:
//! - `{"kind":"live","device","platform","input","mime"}` when it changes; the
//!   binary messages after it are `mime` images or access units in device
//!   pixels;
//! - `{"kind":"idle"}` while no device is up for the task;
//! - `{"kind":"unavailable","message"}` when the host can't capture;
//! - `{"kind":"input_failed","message"}` answers a rejected input.
//!
//! The browser sends `sun_core::host_protocol::DeviceInput` as JSON text.
//! Watching needs any role; input needs `member` (it changes the device).

use std::time::Duration;

use axum::Extension;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::Response;
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use serde_json::{Value, json};
use sun_core::host_protocol::{DeviceInput, DevicePlatform, KernelFrame, ReplyBody};
use sun_core::model::Permission;
use sun_core::{Error, HostId, Result, TaskId};

use crate::Api;
use crate::auth::Caller;
use crate::error::{ApiError, ApiResult};
use crate::hosts::unexpected;
use crate::routes::id;

/// Time between pictures while a device is up (a few fps; each is a
/// screenshot on the host).
const FRAME: Duration = Duration::from_millis(250);
/// Time between looks while no device is up.
const IDLE: Duration = Duration::from_secs(2);
/// Wait for one host answer.
const WAIT: Duration = Duration::from_secs(10);
/// Largest input message (text is capped at 500 characters by the host).
const MAX_INPUT: usize = 4096;

/// `?video=h264`: the browser decodes H.264 (WebCodecs).
#[derive(Deserialize)]
pub(crate) struct View {
    video: Option<String>,
}

/// `GET /api/v1/tasks/{id}/device`: the live view socket.
pub(crate) async fn live(
    State(api): State<Api>,
    Path(task): Path<String>,
    Query(view): Query<View>,
    Extension(caller): Extension<Caller>,
    headers: HeaderMap,
    ws: WebSocketUpgrade,
) -> ApiResult<Response> {
    let task_id: TaskId = id(&task, "task")?;
    same_origin(&headers)?;
    let workspace = api.engine.task(task_id).await?.workspace_id;
    let can_input = caller.can_in(workspace, Permission::Run);
    let video = view.video.as_deref() == Some("h264");
    Ok(ws
        .max_message_size(MAX_INPUT)
        .on_upgrade(move |socket| relay(api, task_id, can_input, video, socket)))
}

/// Cross-site WebSocket hijacking guard: a browser `Origin` must be this
/// host (the session cookie is `SameSite=Strict` too).
fn same_origin(headers: &HeaderMap) -> ApiResult<()> {
    let Some(origin) = headers.get(header::ORIGIN) else {
        return Ok(()); // not a browser
    };
    let host = headers.get(header::HOST).and_then(|h| h.to_str().ok());
    let origin_host = origin
        .to_str()
        .ok()
        .and_then(|o| o.split_once("://"))
        .map(|(_, rest)| rest);
    if host.is_some() && origin_host == host {
        Ok(())
    } else {
        Err(ApiError::new(
            StatusCode::FORBIDDEN,
            "forbidden",
            "The device view only opens from Sun's own page.",
        ))
    }
}

async fn relay(api: Api, task: TaskId, can_input: bool, video: bool, socket: WebSocket) {
    let (mut out, mut incoming) = socket.split();
    let new_stream = || video.then(|| ulid::Ulid::generate().to_string());
    // `Some`: pulling video on this stream; `None`: pictures.
    let mut stream = new_stream();
    let mut streamed = false;
    let mut host = None;
    let mut shown: Option<Value> = None;
    let mut next = Box::pin(look(
        api.clone(),
        task,
        None,
        stream.clone(),
        Duration::ZERO,
    ));
    loop {
        tokio::select! {
            message = incoming.next() => match message {
                Some(Ok(Message::Text(text))) => {
                    if let Err(e) = input(&api, task, host, can_input, &text).await {
                        let failed = json!({"kind": "input_failed", "message": message_of(&e)});
                        if out.send(Message::Text(failed.to_string().into())).await.is_err() {
                            break;
                        }
                    }
                }
                Some(Ok(_)) => {}
                None | Some(Err(_)) => break,
            },
            (found, seen) = &mut next => {
                host = found;
                let (state, payloads, delay) = match seen {
                    Ok(seen) => {
                        streamed |= stream.is_some() && !seen.payloads.is_empty();
                        let state = json!({
                            "kind": "live", "device": seen.device, "platform": seen.platform,
                            "input": seen.input && can_input, "mime": seen.mime,
                        });
                        let delay = if stream.is_some() { Duration::ZERO } else { FRAME };
                        (Some(state), seen.payloads, delay)
                    }
                    Err(Error::NotFound(_)) => {
                        (stream, streamed) = (new_stream(), false);
                        (Some(json!({"kind": "idle"})), Vec::new(), IDLE)
                    }
                    // The encoder ended after streaming (Android's
                    // `screenrecord` stops after 3 min): a fresh stream.
                    // It never streamed: pictures until the device changes.
                    Err(_) if stream.is_some() => {
                        stream = if streamed { new_stream() } else { None };
                        streamed = false;
                        (None, Vec::new(), Duration::ZERO)
                    }
                    Err(e) => {
                        let state = json!({"kind": "unavailable", "message": message_of(&e)});
                        (Some(state), Vec::new(), IDLE)
                    }
                };
                if let Some(state) = state
                    && shown.as_ref() != Some(&state)
                {
                    if out.send(Message::Text(state.to_string().into())).await.is_err() {
                        break;
                    }
                    shown = Some(state);
                }
                for payload in payloads {
                    if out.send(Message::Binary(payload.into())).await.is_err() {
                        return;
                    }
                }
                next = Box::pin(look(api.clone(), task, host, stream.clone(), delay));
            }
        }
    }
}

/// What one look at the task's device brought.
struct Seen {
    device: String,
    platform: DevicePlatform,
    input: bool,
    mime: String,
    /// A picture, or H.264 access units (maybe none).
    payloads: Vec<Vec<u8>>,
}

/// After `delay`, the task's screen from the host that has its device (the
/// last one that had it first, then every online host): video frames on
/// `stream`, else one picture. Also says which host answered.
async fn look(
    api: Api,
    task: TaskId,
    hint: Option<HostId>,
    stream: Option<String>,
    delay: Duration,
) -> (Option<HostId>, Result<Seen>) {
    tokio::time::sleep(delay).await;
    let mut hosts: Vec<HostId> = hint.into_iter().collect();
    hosts.extend(api.hosts.online().into_iter().filter(|h| Some(*h) != hint));
    let mut failure = Error::NotFound(format!("no device is running for task {task}"));
    for candidate in hosts {
        let answer = api
            .hosts
            .request(candidate, WAIT, |id| match &stream {
                Some(stream) => KernelFrame::DeviceVideo {
                    id,
                    task_id: task,
                    stream: stream.clone(),
                },
                None => KernelFrame::DeviceScreen { id, task_id: task },
            })
            .await;
        let seen = match answer {
            Ok(ReplyBody::Screen { screen }) => Seen {
                device: screen.device,
                platform: screen.platform,
                input: screen.input,
                mime: screen.mime,
                payloads: vec![screen.image],
            },
            Ok(ReplyBody::Video { video }) => Seen {
                device: video.device,
                platform: video.platform,
                input: video.input,
                mime: "video/h264".into(),
                payloads: video.frames.into_iter().map(|f| f.data).collect(),
            },
            Ok(other) => {
                failure = unexpected(other);
                continue;
            }
            Err(Error::NotFound(_)) => continue,
            Err(e) => {
                failure = e;
                continue;
            }
        };
        return (Some(candidate), Ok(seen));
    }
    (None, Err(failure))
}

async fn input(
    api: &Api,
    task: TaskId,
    host: Option<HostId>,
    can_input: bool,
    text: &str,
) -> Result<()> {
    if !can_input {
        return Err(Error::InvalidInput(
            "Your role can watch the device but not use it.".into(),
        ));
    }
    let input: DeviceInput = serde_json::from_str(text)
        .map_err(|e| Error::InvalidInput(format!("Not a device input: {e}")))?;
    let host = host.ok_or_else(|| Error::NotFound("No device is running.".into()))?;
    match api
        .hosts
        .request(host, WAIT, |id| KernelFrame::DeviceInput {
            id,
            task_id: task,
            input,
        })
        .await?
    {
        ReplyBody::Done => Ok(()),
        other => Err(unexpected(other)),
    }
}

/// The plain message of an error, without its kind prefix.
fn message_of(error: &Error) -> String {
    match error {
        Error::InvalidInput(m) | Error::NotFound(m) | Error::Unavailable(m) => m.clone(),
        Error::Refused { message, .. } => message.clone(),
        other => other.to_string(),
    }
}
