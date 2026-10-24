//! A terminal on a repo's editor worktree: `GET /api/v1/workspaces/{id}/
//! repos/{repo_id}/terminal?host_id=&cols=&rows=` upgrades to a WebSocket
//! spliced to a shell on that host (`pty.open` in
//! `sun_core::host_protocol`, ADR `docs/adr-editor.md`).
//!
//! **Design:** the kernel asks the host for a PTY over the control socket;
//! the host dials `GET /host/pty` back and this module splices the two
//! sockets as a byte pipe (**Adapter** between browser and host). Binary
//! messages are terminal bytes both ways. Text messages are JSON tagged on
//! `kind`:
//! - browser → kernel: `{"kind":"resize","cols","rows"}`;
//! - kernel → browser: `{"kind":"open","pty_id"}` on every attach,
//!   `{"kind":"exit","code"}` when the shell exits, `{"kind":"dead"}` when
//!   the host's socket dropped, `{"kind":"error","message"}` when no
//!   terminal opened. The socket closes after the last three.
//!
//! A closed browser socket keeps the terminal for the grace period (30 s):
//! `?pty_id=` reattaches it (the same member, repo and host; a second
//! attach replaces the first). After it the host socket closes and the host
//! kills the shell's session. `terminal` is checked on every connect,
//! before anything is spawned; open and close are audited with the
//! duration. Nothing typed or shown is read beyond the `kind` of text
//! messages, and none of it is logged.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, Query, State};
use axum::http::HeaderMap;
use axum::response::Response;
use axum::routing::get;
use axum::{Extension, Router};
use futures_util::stream::{SplitSink, SplitStream};
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use serde_json::{Value, json};
use sun_core::host_protocol::{EditorTree, KernelFrame, PtyControl, PtyHello, ReplyBody};
use sun_core::{Error, HostId, Result, WorkspaceId};
use tokio::sync::{mpsc, oneshot};
use tokio::time::Instant;

use crate::Api;
use crate::auth::Caller;
use crate::device::{message_of, same_origin};
use crate::editor::{Target, target};
use crate::editor_access::{self, Capability, EditorEvent};
use crate::error::ApiResult;
use crate::hosts::{self, FIRST_FRAME, REPLY, unexpected};

/// How long a terminal whose browser left waits for it to come back.
pub(crate) const GRACE: Duration = Duration::from_secs(30);
/// Wait for the host's socket once it answered `pty.open`.
const DIAL: Duration = Duration::from_secs(15);
/// Largest message either way (a paste; output comes in 64 KiB chunks).
const MAX_MESSAGE: usize = 1024 * 1024;

/// Terminals being opened and open, by `pty_id`. Cheap to clone.
#[derive(Clone, Default)]
pub(crate) struct Terminals {
    slots: Arc<Mutex<HashMap<String, Slot>>>,
}

enum Slot {
    /// `pty.open` sent; the host's socket goes here.
    Dialling {
        host: HostId,
        socket: oneshot::Sender<WebSocket>,
    },
    /// Open: a browser socket sent here (re)attaches.
    Live {
        owner: Owner,
        attach: mpsc::Sender<WebSocket>,
    },
}

/// Whose a terminal is: only the same member on the same repo and host
/// reattaches it.
#[derive(Clone, Copy, PartialEq, Eq)]
struct Owner {
    workspace: WorkspaceId,
    host: HostId,
    tree: EditorTree,
}

impl Owner {
    fn of(target: &Target) -> Self {
        Self {
            workspace: target.workspace,
            host: target.host,
            tree: target.tree,
        }
    }
}

impl Terminals {
    fn slots(&self) -> MutexGuard<'_, HashMap<String, Slot>> {
        self.slots.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn put(&self, pty_id: &str, slot: Slot) {
        self.slots().insert(pty_id.to_owned(), slot);
    }

    fn remove(&self, pty_id: &str) {
        self.slots().remove(pty_id);
    }

    /// The waiting end for `host`'s socket of `pty_id`, taken.
    fn dialled(&self, pty_id: &str, host: HostId) -> Option<oneshot::Sender<WebSocket>> {
        let mut slots = self.slots();
        match slots.get(pty_id) {
            Some(Slot::Dialling { host: h, .. }) if *h == host => {}
            _ => return None,
        }
        match slots.remove(pty_id) {
            Some(Slot::Dialling { socket, .. }) => Some(socket),
            _ => None,
        }
    }

    /// Where `owner`'s browser socket goes to reattach `pty_id`.
    fn attach(&self, pty_id: &str, owner: Owner) -> Option<mpsc::Sender<WebSocket>> {
        match self.slots().get(pty_id) {
            Some(Slot::Live { owner: o, attach }) if *o == owner => Some(attach.clone()),
            _ => None,
        }
    }
}

/// `GET /host/pty`: the socket a host dials for a terminal.
pub(crate) fn router(api: Api) -> Router {
    Router::new()
        .route("/host/pty", get(host_upgrade))
        .with_state(api)
}

async fn host_upgrade(State(api): State<Api>, ws: WebSocketUpgrade) -> Response {
    ws.max_message_size(MAX_MESSAGE)
        .on_upgrade(move |socket| host_dialled(api, socket))
}

/// Hands an authentic host's socket to the terminal waiting for it; any
/// other socket is dropped, which the host takes as "kill it".
async fn host_dialled(api: Api, mut socket: WebSocket) {
    let hello = match tokio::time::timeout(FIRST_FRAME, socket.recv()).await {
        Ok(Some(Ok(Message::Text(text)))) => serde_json::from_str::<PtyHello>(&text).ok(),
        _ => None,
    };
    let Some(hello) = hello else { return };
    if !hosts::authentic(&api, hello.host_id, &hello.host_key).await {
        return;
    }
    if let Some(waiting) = api.terminals.dialled(&hello.pty_id, hello.host_id) {
        let _ = waiting.send(socket);
    }
}

/// `?host_id=&cols=&rows=`, or `?host_id=&pty_id=` to reattach.
#[derive(Deserialize)]
pub(crate) struct Connect {
    host_id: String,
    #[serde(default = "default_cols")]
    cols: u16,
    #[serde(default = "default_rows")]
    rows: u16,
    pty_id: Option<String>,
}

fn default_cols() -> u16 {
    80
}

fn default_rows() -> u16 {
    24
}

/// `GET …/repos/{repo_id}/terminal`: 403 without `terminal` there, 404 for
/// an unknown repo, host or (reattaching) terminal; nothing is spawned
/// before these pass.
pub(crate) async fn connect(
    State(api): State<Api>,
    Extension(caller): Extension<Caller>,
    Path(ids): Path<(String, String)>,
    Query(query): Query<Connect>,
    headers: HeaderMap,
    ws: WebSocketUpgrade,
) -> ApiResult<Response> {
    same_origin(&headers)?;
    let target = target(&api, &caller, &ids, &query.host_id, Capability::Terminal).await?;
    let ws = ws.max_message_size(MAX_MESSAGE);
    if let Some(pty_id) = query.pty_id {
        let attach = api
            .terminals
            .attach(&pty_id, Owner::of(&target))
            .ok_or_else(|| Error::NotFound(format!("terminal {pty_id}")))?;
        return Ok(ws.on_upgrade(move |socket| async move {
            let _ = attach.send(socket).await;
        }));
    }
    PtyControl::check_size(query.cols, query.rows)?;
    let (cols, rows) = (query.cols, query.rows);
    Ok(ws.on_upgrade(move |socket| open(api, caller, target, cols, rows, socket)))
}

/// Opens the terminal, audits it, relays it until it ends, audits that.
async fn open(api: Api, caller: Caller, target: Target, cols: u16, rows: u16, browser: WebSocket) {
    let pty_id = ulid::Ulid::generate().to_string();
    let opened = match dial(&api, &target, &pty_id, cols, rows).await {
        Ok(host) => audit(&api, &caller, &target, "terminal.open", "")
            .await
            .map(|()| host),
        Err(e) => Err(e),
    };
    let host = match opened {
        Ok(host) => host,
        Err(e) => {
            api.terminals.remove(&pty_id);
            let mut browser = browser;
            let error = json!({"kind": "error", "message": message_of(&e)});
            let _ = browser.send(control(&error)).await;
            let _ = browser.close().await;
            return;
        }
    };
    let (attach, attached) = mpsc::channel(1);
    let owner = Owner::of(&target);
    api.terminals.put(&pty_id, Slot::Live { owner, attach });
    let started = Instant::now();
    relay(&api, &pty_id, host, browser, attached).await;
    api.terminals.remove(&pty_id);
    let lasted = format!("after {}s", started.elapsed().as_secs());
    if let Err(e) = audit(&api, &caller, &target, "terminal.close", &lasted).await {
        eprintln!("sun-api: audit terminal.close: {e}");
    }
}

/// `pty.open` on the target's host, then its socket.
async fn dial(api: &Api, target: &Target, pty_id: &str, cols: u16, rows: u16) -> Result<WebSocket> {
    let (socket, dialled) = oneshot::channel();
    let host = target.host;
    api.terminals.put(pty_id, Slot::Dialling { host, socket });
    let (tree, id_of_pty) = (target.tree, pty_id.to_owned());
    let reply = api
        .hosts
        .terminal_request(host, REPLY, |id| KernelFrame::PtyOpen {
            id,
            pty_id: id_of_pty,
            tree,
            cols,
            rows,
        })
        .await?;
    if reply != ReplyBody::Done {
        return Err(unexpected(reply));
    }
    match tokio::time::timeout(DIAL, dialled).await {
        Ok(Ok(socket)) => Ok(socket),
        _ => Err(Error::Unavailable(
            "the host opened the terminal but its socket never arrived".into(),
        )),
    }
}

async fn audit(
    api: &Api,
    caller: &Caller,
    target: &Target,
    action: &'static str,
    detail: &str,
) -> Result<()> {
    let event = EditorEvent {
        workspace: target.workspace,
        repo: target.repo.id,
        host: target.host,
        action,
        path: detail,
    };
    editor_access::record(api, caller, event)
        .await
        .map_err(|e| Error::Unavailable(format!("audit: {}", e.message())))
}

/// How a browser attachment ended.
enum End {
    /// The shell exited: its `exit` message.
    Exit(Message),
    /// The host's socket dropped.
    HostGone,
    /// The browser's socket closed or went silent.
    BrowserGone,
    /// Another browser socket reattached.
    Replaced(Option<Box<WebSocket>>),
}

/// Splices `browser` (and every browser that reattaches) to `host` until
/// the shell exits, the host drops or the grace runs out.
async fn relay(
    api: &Api,
    pty_id: &str,
    host: WebSocket,
    mut browser: WebSocket,
    mut attached: mpsc::Receiver<WebSocket>,
) {
    let (ping_every, liveness) = api.host_liveness;
    let (mut to_host, mut from_host) = host.split();
    loop {
        let (mut out, mut incoming) = browser.split();
        let ended = match out
            .send(control(&json!({"kind": "open", "pty_id": pty_id})))
            .await
        {
            Err(_) => End::BrowserGone,
            Ok(()) => tokio::select! {
                end = output(&mut from_host, &mut out, ping_every) => end,
                () = input(&mut incoming, &mut to_host, ping_every, liveness) => End::BrowserGone,
                new = attached.recv() => End::Replaced(new.map(Box::new)),
            },
        };
        let last = match ended {
            End::Exit(exit) => Err(exit),
            End::HostGone => Err(control(&json!({"kind": "dead"}))),
            End::Replaced(new) => Ok(Some(new.map(|new| *new))),
            End::BrowserGone => Ok(None),
        };
        let next = match last {
            Err(last) => {
                let _ = out.send(last).await;
                let _ = out.close().await;
                return;
            }
            Ok(Some(new)) => {
                let _ = out.close().await;
                new
            }
            Ok(None) => park(&mut to_host, &mut attached, api.terminal_grace, ping_every).await,
        };
        match next {
            Some(next) => browser = next,
            None => {
                let _ = to_host.close().await;
                return;
            }
        }
    }
}

/// Host → browser, pinging the browser, until the shell exits, the host
/// drops or the browser can't be written.
async fn output(
    host: &mut SplitStream<WebSocket>,
    browser: &mut SplitSink<WebSocket, Message>,
    ping_every: Duration,
) -> End {
    let mut ping = tokio::time::interval_at(Instant::now() + ping_every, ping_every);
    loop {
        let message = tokio::select! {
            message = host.next() => match message {
                Some(Ok(Message::Binary(bytes))) => Message::Binary(bytes),
                Some(Ok(Message::Text(text))) => {
                    if let Ok(PtyControl::Exit { .. }) = serde_json::from_str(&text) {
                        return End::Exit(Message::Text(text));
                    }
                    continue;
                }
                Some(Ok(Message::Close(_)) | Err(_)) | None => return End::HostGone,
                Some(Ok(_)) => continue,
            },
            _ = ping.tick() => Message::Ping(Default::default()),
        };
        if browser.send(message).await.is_err() {
            return End::BrowserGone;
        }
    }
}

/// Browser → host (bytes and checked resizes), pinging the host, until the
/// browser closes or is silent for `liveness`.
async fn input(
    browser: &mut SplitStream<WebSocket>,
    host: &mut SplitSink<WebSocket, Message>,
    ping_every: Duration,
    liveness: Duration,
) {
    let mut ping = tokio::time::interval_at(Instant::now() + ping_every, ping_every);
    let mut heard = Instant::now();
    loop {
        let message = tokio::select! {
            message = browser.next() => {
                heard = Instant::now();
                match message {
                    Some(Ok(Message::Binary(bytes))) => Message::Binary(bytes),
                    Some(Ok(Message::Text(text))) => match serde_json::from_str(&text) {
                        Ok(resize @ PtyControl::Resize { cols, rows })
                            if PtyControl::check_size(cols, rows).is_ok() =>
                        {
                            Message::Text(serde_json::to_string(&resize).unwrap_or_default().into())
                        }
                        _ => continue,
                    },
                    Some(Ok(Message::Close(_)) | Err(_)) | None => return,
                    Some(Ok(_)) => continue, // pongs
                }
            }
            _ = ping.tick() => Message::Ping(Default::default()),
            () = tokio::time::sleep_until(heard + liveness) => return,
        };
        if host.send(message).await.is_err() {
            return;
        }
    }
}

/// Waits up to `grace` for a browser to reattach, keeping the host socket
/// alive; `None` when none did or the host dropped.
async fn park(
    host: &mut SplitSink<WebSocket, Message>,
    attached: &mut mpsc::Receiver<WebSocket>,
    grace: Duration,
    ping_every: Duration,
) -> Option<WebSocket> {
    let deadline = tokio::time::sleep(grace);
    tokio::pin!(deadline);
    let mut ping = tokio::time::interval_at(Instant::now() + ping_every, ping_every);
    loop {
        tokio::select! {
            next = attached.recv() => return next,
            () = &mut deadline => return None,
            _ = ping.tick() => {
                if host.send(Message::Ping(Default::default())).await.is_err() {
                    return None;
                }
            }
        }
    }
}

fn control(value: &Value) -> Message {
    Message::Text(value.to_string().into())
}
