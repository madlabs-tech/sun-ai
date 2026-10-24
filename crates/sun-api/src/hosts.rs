//! Kernel side of the host protocol (`sun_core::host_protocol`): the
//! `GET /host` WebSocket and [`HostHub`], the **Adapter** that implements
//! `sun_core::HostLink` over the live host connections. Also the host and
//! runtime lists and enrolment tokens of the API.

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use axum::extract::State;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::http::StatusCode;
use axum::response::Response;
use axum::routing::get;
use axum::{Json, Router};
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use sun_core::host_protocol::{
    DeviceCapabilities, DeviceFlowRequest, DeviceInput, DeviceRequest, DeviceResult, DeviceScreen,
    EnrolFailure, ExecOutput, ExecRequest, HOST_PROTOCOL_VERSION, HostFrame, KernelFrame,
    ReplyBody, ScreenshotRequest, SessionFile, SessionStats, Worktree,
};
use sun_core::model::{Credential, RuntimeSource};
use sun_core::{
    BoxStream, Error, HostEvent, HostId, HostLink, MemberId, Readiness, Result, RunId,
    SessionHandle, SessionId, SessionSpec, Steer, StepKey, StopMode, TaskId, Timestamp,
    WorkspaceId,
};
use sun_engine::OnWorkspace;
use tokio::sync::{mpsc, oneshot};

use crate::entitlement::within_plan;
use crate::error::{ApiError, ApiResult};
use crate::workspace_access::{InWorkspace, listed_workspace, named_workspace, visible};
use crate::{Api, auth, wire};
use sun_core::entitlement::Quota;

/// A host must send its first frame within this.
pub(crate) const FIRST_FRAME: Duration = Duration::from_secs(10);
/// Default wait for a reply.
pub(crate) const REPLY: Duration = Duration::from_secs(60);
/// `worktree.prepare` (and `editor.open`) may clone a repo.
pub(crate) const PREPARE: Duration = Duration::from_secs(15 * 60);
/// A host is shown with a session while its stats are this fresh.
const STATS_FRESH: Duration = Duration::from_secs(15);
const TOKEN_TTL: Duration = Duration::from_secs(60 * 60);
const MAX_FRAME: usize = 16 * 1024 * 1024;
/// Buffered host events per `events()` subscriber before the reader waits.
const EVENT_BUFFER: usize = 1024;
/// How often the kernel pings each host connection.
pub(crate) const PING_EVERY: Duration = Duration::from_secs(10);
/// A host connection silent this long (no frame, no pong) is dropped and
/// the host shows offline.
pub(crate) const LIVENESS: Duration = Duration::from_secs(30);

/// Live connections of every host (**Adapter** for `HostLink`). Cheap to
/// clone; create it before the engine and give the same hub to both the
/// engine (as its `HostLink`) and [`crate::Api`].
#[derive(Clone, Default)]
pub struct HostHub {
    inner: Arc<Hub>,
}

#[derive(Default)]
struct Hub {
    state: Mutex<HubState>,
    next_id: AtomicU64,
}

#[derive(Default)]
struct HubState {
    /// The newest connection of each online host.
    conns: HashMap<HostId, Conn>,
    /// Requests waiting for a reply: id → (host, connection generation, reply).
    pending: HashMap<u64, (HostId, u64, oneshot::Sender<ReplyBody>)>,
    /// Which host runs a session (from `session.start`, `hello` and events).
    sessions: HashMap<SessionId, HostId>,
    stats: HashMap<SessionId, (SessionStats, Instant)>,
    /// The run step each session started here works for (`session.start`).
    steps: HashMap<SessionId, (RunId, StepKey)>,
    /// The adapter each session started here runs (`session.start`).
    adapters: HashMap<SessionId, String>,
    /// Bytes per task folder, from each host's latest `disk` frame.
    disk: HashMap<HostId, Vec<(TaskId, u64)>>,
    last_seen: HashMap<HostId, Timestamp>,
    subscribers: Vec<mpsc::Sender<HostEvent>>,
    generation: u64,
}

/// What an enrolment token enrols.
#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Enrols {
    /// A host attached to the workspace, enrolled by who minted the token.
    Workspace(WorkspaceId, Option<MemberId>),
    /// A leased seller host serving only this marketplace task.
    Market(TaskId),
}

struct Conn {
    generation: u64,
    out: mpsc::UnboundedSender<KernelFrame>,
    /// The host's protocol minor: 1+ speaks `reprobe` and `capabilities`,
    /// 2+ `editor.open` and `fs.*`, 3+ `pty.open`.
    minor: u32,
}

impl HostHub {
    // A poisoned lock only means another task panicked mid-update of plain
    // maps; keep serving.
    fn state(&self) -> MutexGuard<'_, HubState> {
        self.inner
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn attach(&self, host: HostId, out: mpsc::UnboundedSender<KernelFrame>, protocol: &str) -> u64 {
        let mut state = self.state();
        state.generation += 1;
        let generation = state.generation;
        let minor = protocol
            .split('.')
            .nth(1)
            .and_then(|m| m.parse::<u32>().ok())
            .unwrap_or(0);
        // The newest connection wins; dropping the old sender ends its writer.
        state.conns.insert(
            host,
            Conn {
                generation,
                out,
                minor,
            },
        );
        state.last_seen.insert(host, chrono::Utc::now());
        generation
    }

    fn detach(&self, host: HostId, generation: u64) {
        let mut state = self.state();
        if state
            .conns
            .get(&host)
            .is_some_and(|c| c.generation == generation)
        {
            state.conns.remove(&host);
        }
        // Replies to requests sent on this connection will never come.
        state
            .pending
            .retain(|_, (h, g, _)| !(*h == host && *g == generation));
    }

    fn host_of(&self, session: SessionId) -> Result<HostId> {
        self.state()
            .sessions
            .get(&session)
            .copied()
            .ok_or_else(|| Error::Unavailable(format!("session {session}: host not connected")))
    }

    /// Hands a host event to every `events()` stream. Waits when a consumer
    /// is behind, which back-pressures that host's socket.
    async fn publish(&self, event: HostEvent) {
        let subscribers = self.state().subscribers.clone();
        let mut closed = false;
        for subscriber in subscribers {
            closed |= subscriber.send(event.clone()).await.is_err();
        }
        if closed {
            self.state().subscribers.retain(|s| !s.is_closed());
        }
    }

    /// The hosts connected right now.
    pub(crate) fn online(&self) -> Vec<HostId> {
        self.state().conns.keys().copied().collect()
    }

    /// When `host` was last heard from (`None`: not since the kernel started).
    pub(crate) fn last_seen(&self, host: HostId) -> Option<Timestamp> {
        self.state().last_seen.get(&host).copied()
    }

    /// Live sessions per `(host, adapter)`: those started here whose stats
    /// are fresh, as `GET /hosts` counts them. A session the kernel learned
    /// only from a `hello` (after a restart) has no known adapter and is
    /// not counted.
    pub(crate) fn active_sessions(&self) -> HashMap<(HostId, String), u32> {
        let state = self.state();
        let now = Instant::now();
        let mut active = HashMap::new();
        for (session, (_, at)) in &state.stats {
            if now.duration_since(*at) >= STATS_FRESH {
                continue;
            }
            if let (Some(host), Some(adapter)) =
                (state.sessions.get(session), state.adapters.get(session))
            {
                *active.entry((*host, adapter.clone())).or_default() += 1;
            }
        }
        active
    }

    /// Sends one request to `host` and waits up to `wait` for its reply.
    pub(crate) async fn request(
        &self,
        host: HostId,
        wait: Duration,
        frame: impl FnOnce(u64) -> KernelFrame,
    ) -> Result<ReplyBody> {
        let id = self.inner.next_id.fetch_add(1, Ordering::Relaxed);
        let (reply, answer) = oneshot::channel();
        {
            let mut state = self.state();
            let conn = state
                .conns
                .get(&host)
                .ok_or_else(|| Error::Unavailable(format!("host {host} is offline")))?;
            let generation = conn.generation;
            conn.out
                .send(frame(id))
                .map_err(|_| Error::Unavailable(format!("host {host} is offline")))?;
            state.pending.insert(id, (host, generation, reply));
        }
        match tokio::time::timeout(wait, answer).await {
            Ok(Ok(body)) => body.into_result(),
            Ok(Err(_)) => Err(Error::Unavailable(format!("host {host} disconnected"))),
            Err(_) => {
                self.state().pending.remove(&id);
                Err(Error::Unavailable(format!("host {host} did not answer")))
            }
        }
    }

    /// Has `host` probe its runtimes again now and waits until it did; a
    /// change arrives (and is stored) before the reply.
    pub(crate) async fn reprobe(&self, host: HostId) -> Result<()> {
        let reprobes = self.state().conns.get(&host).map(|c| c.minor >= 1);
        match reprobes {
            None => Err(Error::Unavailable(format!("host {host} is offline"))),
            Some(false) => Err(Error::Unavailable(
                "this host's sun is too old to check again; update it, or restart it to re-probe"
                    .into(),
            )),
            Some(true) => match self
                .request(host, REPLY, |id| KernelFrame::Reprobe { id })
                .await?
            {
                ReplyBody::Done => Ok(()),
                other => Err(unexpected(other)),
            },
        }
    }

    /// [`Self::request`] for an editor frame; `Unavailable` at once when
    /// the host's sun predates the editor (protocol 2.2).
    pub(crate) async fn editor_request(
        &self,
        host: HostId,
        wait: Duration,
        frame: impl FnOnce(u64) -> KernelFrame,
    ) -> Result<ReplyBody> {
        self.request_since(2, "the editor", host, wait, frame).await
    }

    /// [`Self::request`] for `pty.open`; `Unavailable` at once when the
    /// host's sun predates terminals (protocol 2.3).
    pub(crate) async fn terminal_request(
        &self,
        host: HostId,
        wait: Duration,
        frame: impl FnOnce(u64) -> KernelFrame,
    ) -> Result<ReplyBody> {
        self.request_since(3, "the terminal", host, wait, frame)
            .await
    }

    async fn request_since(
        &self,
        minor: u32,
        feature: &str,
        host: HostId,
        wait: Duration,
        frame: impl FnOnce(u64) -> KernelFrame,
    ) -> Result<ReplyBody> {
        if self
            .state()
            .conns
            .get(&host)
            .is_some_and(|c| c.minor < minor)
        {
            return Err(Error::Unavailable(format!(
                "this host's sun is too old for {feature}; update it"
            )));
        }
        self.request(host, wait, frame).await
    }

    async fn session_request(
        &self,
        session: SessionId,
        frame: impl FnOnce(u64) -> KernelFrame,
    ) -> Result<ReplyBody> {
        let host = self.host_of(session)?;
        self.request(host, REPLY, frame).await
    }
}

pub(crate) fn unexpected(body: ReplyBody) -> Error {
    Error::Protocol(format!("unexpected reply {body:?}"))
}

#[async_trait]
impl HostLink for HostHub {
    async fn start_session(
        &self,
        host: HostId,
        adapter: &str,
        spec: SessionSpec,
    ) -> Result<SessionHandle> {
        // Route the session before its first event can arrive.
        {
            let mut state = self.state();
            state.sessions.insert(spec.session_id, host);
            let step = (spec.run_id, spec.step_key.clone());
            state.steps.insert(spec.session_id, step);
            state.adapters.insert(spec.session_id, adapter.to_string());
        }
        let adapter = adapter.to_string();
        match self
            .request(host, REPLY, |id| KernelFrame::SessionStart {
                id,
                adapter,
                spec: Box::new(spec),
            })
            .await?
        {
            ReplyBody::Session { handle } => Ok(handle),
            other => Err(unexpected(other)),
        }
    }

    async fn steer(&self, session: &SessionHandle, msg: Steer) -> Result<()> {
        let session_id = session.session_id;
        match self
            .session_request(session_id, |id| KernelFrame::SessionSend {
                id,
                session_id,
                text: msg.text,
            })
            .await?
        {
            ReplyBody::Done => Ok(()),
            other => Err(unexpected(other)),
        }
    }

    async fn stop_session(&self, session: &SessionHandle, how: StopMode) -> Result<()> {
        let session_id = session.session_id;
        match self
            .session_request(session_id, |id| KernelFrame::SessionStop {
                id,
                session_id,
                how,
            })
            .await?
        {
            ReplyBody::Done => Ok(()),
            other => Err(unexpected(other)),
        }
    }

    /// `NotFound` when no connected host has the session: after a kernel
    /// restart the hub learns sessions only from each host's `hello`.
    async fn is_alive(&self, session: &SessionHandle) -> Result<bool> {
        let session_id = session.session_id;
        if !self.state().sessions.contains_key(&session_id) {
            return Err(Error::NotFound(format!(
                "session {session_id} is on no connected host"
            )));
        }
        match self
            .session_request(session_id, |id| KernelFrame::Probe { id, session_id })
            .await?
        {
            ReplyBody::Alive { alive } => Ok(alive),
            other => Err(unexpected(other)),
        }
    }

    fn events(&self) -> BoxStream<'static, HostEvent> {
        let (sender, mut receiver) = mpsc::channel(EVENT_BUFFER);
        self.state().subscribers.push(sender);
        Box::pin(futures_util::stream::poll_fn(move |cx| {
            receiver.poll_recv(cx)
        }))
    }

    async fn ack(&self, session: SessionId, seq: u64) -> Result<()> {
        let host = self.host_of(session)?;
        let state = self.state();
        let conn = state
            .conns
            .get(&host)
            .ok_or_else(|| Error::Unavailable(format!("host {host} is offline")))?;
        conn.out
            .send(KernelFrame::Ack {
                session_id: session,
                seq,
            })
            .map_err(|_| Error::Unavailable(format!("host {host} is offline")))
    }

    fn is_online(&self, host: HostId) -> bool {
        self.state().conns.contains_key(&host)
    }

    async fn prepare_worktree(&self, host: HostId, worktree: Worktree) -> Result<PathBuf> {
        match self
            .request(host, PREPARE, |id| KernelFrame::WorktreePrepare {
                id,
                worktree,
            })
            .await?
        {
            ReplyBody::Worktree { path } => Ok(path),
            other => Err(unexpected(other)),
        }
    }

    async fn remove_worktree(&self, host: HostId, task_id: TaskId) -> Result<()> {
        match self
            .request(host, PREPARE, |id| KernelFrame::WorktreeRemove {
                id,
                task_id,
            })
            .await?
        {
            ReplyBody::Done => Ok(()),
            other => Err(unexpected(other)),
        }
    }

    async fn write_files(&self, host: HostId, cwd: PathBuf, files: Vec<SessionFile>) -> Result<()> {
        match self
            .request(host, REPLY, |id| KernelFrame::WorktreeWrite {
                id,
                cwd,
                files,
            })
            .await?
        {
            ReplyBody::Done => Ok(()),
            other => Err(unexpected(other)),
        }
    }

    async fn exec(&self, host: HostId, request: ExecRequest) -> Result<ExecOutput> {
        let wait = Duration::from_secs(request.timeout_s.saturating_add(30));
        match self
            .request(host, wait, |id| KernelFrame::Exec { id, request })
            .await?
        {
            ReplyBody::Exec { output } => Ok(output),
            other => Err(unexpected(other)),
        }
    }

    async fn verify_on_device(&self, host: HostId, request: DeviceRequest) -> Result<DeviceResult> {
        let wait = Duration::from_secs(request.timeout_s.saturating_add(120));
        match self
            .request(host, wait, |id| KernelFrame::DeviceVerify { id, request })
            .await?
        {
            ReplyBody::Device { result } => Ok(result),
            other => Err(unexpected(other)),
        }
    }

    async fn device_screen(&self, host: HostId, task_id: TaskId) -> Result<DeviceScreen> {
        let wait = Duration::from_secs(30);
        match self
            .request(host, wait, |id| KernelFrame::DeviceScreen { id, task_id })
            .await?
        {
            ReplyBody::Screen { screen } => Ok(screen),
            other => Err(unexpected(other)),
        }
    }

    async fn device_input(&self, host: HostId, task_id: TaskId, input: DeviceInput) -> Result<()> {
        let wait = Duration::from_secs(30);
        match self
            .request(host, wait, |id| KernelFrame::DeviceInput {
                id,
                task_id,
                input,
            })
            .await?
        {
            ReplyBody::Done => Ok(()),
            other => Err(unexpected(other)),
        }
    }

    async fn device_flow(&self, host: HostId, request: DeviceFlowRequest) -> Result<DeviceResult> {
        let wait = Duration::from_secs(request.timeout_s.saturating_add(120));
        match self
            .request(host, wait, |id| KernelFrame::DeviceFlow { id, request })
            .await?
        {
            ReplyBody::Device { result } => Ok(result),
            other => Err(unexpected(other)),
        }
    }

    /// May first start the side's dev server, like `preview.start`.
    async fn preview_screenshot(
        &self,
        host: HostId,
        request: ScreenshotRequest,
    ) -> Result<Vec<u8>> {
        match self
            .request(host, PREPARE, |id| KernelFrame::PreviewScreenshot {
                id,
                request,
            })
            .await?
        {
            ReplyBody::Png { png } => Ok(png),
            other => Err(unexpected(other)),
        }
    }

    async fn stats(&self, session: SessionId) -> Result<SessionStats> {
        self.state()
            .stats
            .get(&session)
            .map(|(stats, _)| stats.clone())
            .ok_or_else(|| Error::NotFound(format!("no stats for session {session} yet")))
    }
}

pub(crate) fn router(api: Api) -> Router {
    Router::new().route("/host", get(upgrade)).with_state(api)
}

async fn upgrade(State(api): State<Api>, ws: WebSocketUpgrade) -> Response {
    ws.max_message_size(MAX_FRAME)
        .on_upgrade(move |socket| connection(api, socket))
}

/// The next host frame; `None` on close, error, a frame that does not
/// parse, or `idle` without any message (a pong counts: the host is there).
async fn next_frame(
    socket: &mut futures_util::stream::SplitStream<WebSocket>,
    idle: Duration,
) -> Option<HostFrame> {
    loop {
        match tokio::time::timeout(idle, socket.next())
            .await
            .ok()??
            .ok()?
        {
            Message::Text(text) => return serde_json::from_str(text.as_str()).ok(),
            Message::Close(_) => return None,
            // Binary is ignored (protocol); pings are answered by axum.
            _ => {}
        }
    }
}

async fn connection(api: Api, socket: WebSocket) {
    let (mut sink, mut stream) = socket.split();
    let first = tokio::time::timeout(FIRST_FRAME, next_frame(&mut stream, FIRST_FRAME)).await;
    match first {
        Ok(Some(HostFrame::Enroll { token, name })) => {
            if let Some(frame) = enroll(&api, &token, &name).await {
                let _ = sink.send(text(&frame)).await;
            }
            let _ = sink.close().await;
        }
        Ok(Some(HostFrame::Status { host_id, host_key })) => {
            if authentic(&api, host_id, &host_key).await {
                let connected = api.hosts.online().contains(&host_id);
                let workspaces = workspace_names(&api, host_id).await;
                if let Ok(workspaces) = workspaces {
                    let status = KernelFrame::Status {
                        connected,
                        workspaces,
                    };
                    let _ = sink.send(text(&status)).await;
                }
            }
            let _ = sink.close().await;
        }
        Ok(Some(HostFrame::Leave { host_id, host_key })) => {
            if authentic(&api, host_id, &host_key).await && leave(&api, host_id).await.is_ok() {
                let _ = sink.send(text(&KernelFrame::Left)).await;
            }
            let _ = sink.close().await;
        }
        Ok(Some(HostFrame::Hello {
            host_id,
            host_key,
            protocol,
            capabilities,
            last_ack,
        })) => {
            let major = protocol.split('.').next();
            if major != HOST_PROTOCOL_VERSION.split('.').next()
                || !authentic(&api, host_id, &host_key).await
            {
                let _ = sink.close().await;
                return;
            }
            if let Ok(capabilities) = serde_json::to_value(&capabilities) {
                let _ = api
                    .engine
                    .set_host_capabilities(host_id, &capabilities)
                    .await;
            }
            let (out, mut outgoing) = mpsc::unbounded_channel();
            let hub = &api.hosts;
            {
                let mut state = hub.state();
                for session in last_ack.keys() {
                    state.sessions.insert(*session, host_id);
                }
            }
            let generation = hub.attach(host_id, out, &protocol);
            // A host that joins (or comes back) may take queued work.
            api.queue.wake();
            let (ping_every, liveness) = api.host_liveness;
            let writer = tokio::spawn(async move {
                let first = tokio::time::Instant::now() + ping_every;
                let mut ping = tokio::time::interval_at(first, ping_every);
                loop {
                    let message = tokio::select! {
                        frame = outgoing.recv() => match frame {
                            Some(frame) => text(&frame),
                            // Replaced by a newer connection of the same host.
                            None => break,
                        },
                        _ = ping.tick() => Message::Ping(Default::default()),
                    };
                    if sink.send(message).await.is_err() {
                        return;
                    }
                }
                let _ = sink.close().await;
            });
            // A host that stops answering pings is dropped: offline.
            while let Some(frame) = next_frame(&mut stream, liveness).await {
                hub.state().last_seen.insert(host_id, chrono::Utc::now());
                match frame {
                    HostFrame::Event(event) => {
                        hub.state().sessions.insert(event.session_id, host_id);
                        hub.publish(event).await;
                    }
                    HostFrame::Stats(stats) => {
                        hub.state()
                            .stats
                            .insert(stats.session_id, (stats, Instant::now()));
                    }
                    HostFrame::Disk(disk) => {
                        hub.state().disk.insert(host_id, disk.tasks);
                    }
                    // A re-probe saw a change: a login, a logout, a new CLI.
                    // Stored as `hello`'s are; effort tokens are the host's
                    // report, checked again where an agent picks one.
                    HostFrame::Capabilities { capabilities } => {
                        if let Ok(capabilities) = serde_json::to_value(&capabilities) {
                            let _ = api
                                .engine
                                .set_host_capabilities(host_id, &capabilities)
                                .await;
                        }
                        api.queue.wake();
                    }
                    HostFrame::Reply { id, body } => {
                        let waiting = hub.state().pending.remove(&id);
                        if let Some((_, _, reply)) = waiting {
                            let _ = reply.send(body);
                        }
                    }
                    // A second first frame is a protocol violation.
                    HostFrame::Enroll { .. }
                    | HostFrame::Hello { .. }
                    | HostFrame::Status { .. }
                    | HostFrame::Leave { .. } => break,
                }
            }
            hub.detach(host_id, generation);
            writer.abort();
        }
        // Anything else first (or nothing in time): close without a word.
        _ => {
            let _ = sink.close().await;
        }
    }
}

/// `host_id` is enrolled here and `host_key` is its key.
pub(crate) async fn authentic(api: &Api, host_id: HostId, host_key: &str) -> bool {
    let key = host_key_for(&api.config.host_secret, host_id);
    api.engine.host(host_id).await.is_ok() && auth::same(host_key.as_bytes(), key.as_bytes())
}

/// The names of the workspaces `host` is attached to.
async fn workspace_names(api: &Api, host: HostId) -> Result<Vec<String>> {
    let item = sun_engine::WorkspaceItem::Host(host);
    let mut names = Vec::new();
    for workspace in api.engine.workspaces_of(item).await? {
        names.push(api.engine.workspace(workspace).await?.name);
    }
    Ok(names)
}

/// `sun host leave`: detaches `host` from every workspace (audited as the
/// host). Its row stays, so past runs still name it.
async fn leave(api: &Api, host: HostId) -> Result<()> {
    let item = sun_engine::WorkspaceItem::Host(host);
    for workspace in api.engine.workspaces_of(item).await? {
        api.engine
            .detach_from_workspace(workspace, item, &format!("host:{host}"))
            .await?;
    }
    Ok(())
}

/// A new single-use enrolment token for what it `enrols`, and when it
/// expires (in an hour). Stored by hash, so it outlives a kernel restart.
pub(crate) async fn issue_token(api: &Api, enrols: Enrols) -> Result<(String, Timestamp)> {
    let token = random_hex()?;
    let expires_at = chrono::Utc::now() + TOKEN_TTL;
    let enrols = serde_json::to_value(enrols).map_err(|e| Error::Unavailable(e.to_string()))?;
    api.engine
        .add_host_token(&sha256_hex(&token), &enrols, expires_at)
        .await?;
    Ok((token, expires_at))
}

/// Trades a valid token for a new host and its key, or says why not
/// (`enrol_failed`); `None` closes silently (a kernel error).
async fn enroll(api: &Api, token: &str, name: &str) -> Option<KernelFrame> {
    let refuse = |reason| Some(KernelFrame::EnrolFailed { reason });
    let name = name.trim();
    if name.is_empty() || name.len() > 64 || name.chars().any(char::is_control) {
        return refuse(EnrolFailure::BadName);
    }
    let Some((enrols, expires_at)) = api.engine.take_host_token(&sha256_hex(token)).await.ok()?
    else {
        return refuse(EnrolFailure::InvalidToken);
    };
    if expires_at <= chrono::Utc::now() {
        return refuse(EnrolFailure::Expired);
    }
    // A token minted before the plan filled up still can't pass it. A
    // leased seller host is paid capacity, not one of the plan's own hosts.
    let host = match serde_json::from_value(enrols).ok()? {
        Enrols::Workspace(workspace, by) => {
            let plan = api.entitlements.for_workspace(workspace).await;
            let name = name.to_string();
            match api
                .engine
                .add_host(name, Some((workspace, &plan)), by)
                .await
            {
                Ok(host) => host,
                Err(Error::PlanLimit(_)) => return refuse(EnrolFailure::PlanLimit),
                Err(_) => return None,
            }
        }
        Enrols::Market(task) => {
            let host = api
                .engine
                .add_host(name.to_string(), None, None)
                .await
                .ok()?;
            api.engine.bind_market_host(task, host.id).await.ok()?;
            host
        }
    };
    Some(KernelFrame::Enrolled {
        host_id: host.id,
        host_key: host_key_for(&api.config.host_secret, host.id),
    })
}

fn text(frame: &KernelFrame) -> Message {
    Message::Text(serde_json::to_string(frame).unwrap_or_default().into())
}

/// `hex(sha256(secret ‖ ":" ‖ host_id))`: nothing per host to store or leak.
/// Public so `sun serve --with-host` can credential its own host up front.
pub fn host_key_for(secret: &str, host: HostId) -> String {
    sha256_hex(&format!("{secret}:{host}"))
}

fn sha256_hex(text: &str) -> String {
    let digest = Sha256::digest(text.as_bytes());
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

pub(crate) fn random_hex() -> Result<String> {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).map_err(|e| Error::Unavailable(format!("no randomness: {e}")))?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

/// 403 `plan_limit` when `workspace`'s plan has no room for another host
/// (`limits.hosts`, counting the hosts attached to it).
async fn within_hosts(api: &Api, workspace: WorkspaceId) -> ApiResult<()> {
    let attached = api.engine.hosts(Some(workspace)).await?.len();
    within_plan(api, workspace, Quota::Hosts, attached).await
}

/// `POST /api/v1/workspaces/{id}/hosts/tokens` (`manage_runtimes` there): a
/// single-use token for `sun host join`, with the `kernel_url` it joins; the
/// host it enrols is attached to the workspace. Hosts count against its plan (`limits.hosts`).
pub(crate) async fn create_token(
    State(api): State<Api>,
    axum::Extension(caller): axum::Extension<auth::Caller>,
    axum::extract::Path(workspace): axum::extract::Path<String>,
) -> ApiResult<(StatusCode, Json<Value>)> {
    let workspace = crate::routes::id(&workspace, "workspace")?;
    within_hosts(&api, workspace).await?;
    let (token, expires_at) =
        issue_token(&api, Enrols::Workspace(workspace, caller.member_id)).await?;
    Ok((
        StatusCode::CREATED,
        Json(json!({
            "token": token,
            "expires_at": expires_at,
            "kernel_url": api.config.public_url.trim_end_matches('/'),
        })),
    ))
}

/// `PUT /api/v1/workspaces/{id}/hosts/{host}` (`manage_runtimes` there):
/// attaches an enrolled host, so the workspace's runs may go to it. Only a
/// deployment owner, or the member who enrolled the host, may (anyone else
/// gets 404, as for a missing host). Counts against the plan; audited.
pub(crate) async fn attach_host(
    State(api): State<Api>,
    axum::Extension(caller): axum::Extension<auth::Caller>,
    axum::extract::Path((workspace, host)): axum::extract::Path<(String, String)>,
) -> ApiResult<StatusCode> {
    let workspace = crate::routes::id(&workspace, "workspace")?;
    let host = crate::routes::id::<HostId>(&host, "host")?;
    let row = api.engine.host(host).await?;
    let enrolled_it = caller.member_id.is_some() && row.enrolled_by == caller.member_id;
    if !caller.is_owner && !enrolled_it {
        return Err(Error::NotFound(format!("host {host}")).into());
    }
    let plan = api.entitlements.for_workspace(workspace).await;
    let item = sun_engine::WorkspaceItem::Host(host);
    api.engine
        .import_to_workspace(workspace, item, &caller.actor(), &plan)
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

/// `DELETE /api/v1/workspaces/{id}/hosts/{host}` (`manage_runtimes` there):
/// the workspace's runs no longer go to the host (the ones on it go on);
/// 404 when it isn't attached. Audited.
pub(crate) async fn detach_host(
    State(api): State<Api>,
    axum::Extension(caller): axum::Extension<auth::Caller>,
    axum::extract::Path((workspace, host)): axum::extract::Path<(String, String)>,
) -> ApiResult<StatusCode> {
    let workspace = crate::routes::id(&workspace, "workspace")?;
    let host = crate::routes::id::<HostId>(&host, "host")?;
    let item = sun_engine::WorkspaceItem::Host(host);
    api.engine
        .detach_from_workspace(workspace, item, &caller.actor())
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

/// `POST /api/v1/workspaces/{id}/hosts/{host}/reprobe` (`manage_runtimes`
/// there): "Check again". The host re-probes its runtimes now; 204 once it
/// did, with any change already stored, so the next `GET /hosts` or
/// `GET /runtimes` shows it. 404 when the host isn't attached there, 503 when
/// it is offline or its sun predates re-probing.
pub(crate) async fn reprobe_host(
    State(api): State<Api>,
    axum::extract::Path((workspace, host)): axum::extract::Path<(String, String)>,
) -> ApiResult<StatusCode> {
    let workspace = crate::routes::id(&workspace, "workspace")?;
    let host = crate::routes::id::<HostId>(&host, "host")?;
    let item = sun_engine::WorkspaceItem::Host(host);
    if !api.engine.workspaces_of(item).await?.contains(&workspace) {
        return Err(Error::NotFound(format!("host {host}")).into());
    }
    api.hosts.reprobe(host).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// `GET /api/v1/hosts?workspace=`: the hosts attached to the workspace (any
/// member there); a deployment owner may leave it out for every host. Each
/// names the workspaces it is attached to (those the caller sees) and who
/// enrolled it; sessions and disk use show only the caller's workspaces'.
pub(crate) async fn list_hosts(
    State(api): State<Api>,
    axum::Extension(caller): axum::Extension<auth::Caller>,
    axum::extract::Query(query): axum::extract::Query<InWorkspace>,
) -> ApiResult<Json<Vec<Value>>> {
    let workspace = listed_workspace(&caller, query.workspace.as_deref())?;
    let hosts = api.engine.hosts(workspace).await.map_err(ApiError::from)?;
    let mut attached = Vec::with_capacity(hosts.len());
    for host in &hosts {
        let item = sun_engine::WorkspaceItem::Host(host.id);
        let mut workspaces = api.engine.workspaces_of(item).await?;
        workspaces.retain(|w| caller.sees(*w));
        attached.push(workspaces);
    }
    // A shared host runs other workspaces' sessions too: their run and task
    // ids are not this caller's to see.
    let (runs, tasks): (Vec<RunId>, Vec<TaskId>) = {
        let state = api.hosts.state();
        (
            state.steps.values().map(|(run, _)| *run).collect(),
            state
                .disk
                .values()
                .flatten()
                .map(|(task, _)| *task)
                .collect(),
        )
    };
    let mut hidden_runs = std::collections::HashSet::new();
    for run in runs {
        if !visible(&api, &caller, OnWorkspace::Run(run)).await? {
            hidden_runs.insert(run);
        }
    }
    let mut hidden_tasks = std::collections::HashSet::new();
    for task in tasks {
        if !visible(&api, &caller, OnWorkspace::Task(task)).await? {
            hidden_tasks.insert(task);
        }
    }
    let state = api.hosts.state();
    let now = Instant::now();
    let views = hosts
        .into_iter()
        .zip(attached)
        .map(|(host, workspace_ids)| {
            let caps = &host.capabilities;
            let mut sessions: Vec<Value> = state
                .stats
                .iter()
                .filter(|(session, (_, at))| {
                    now.duration_since(*at) < STATS_FRESH
                        && state.sessions.get(session) == Some(&host.id)
                        && state
                            .steps
                            .get(session)
                            .is_none_or(|(run, _)| !hidden_runs.contains(run))
                })
                .map(|(session, (stats, _))| {
                    let step = state.steps.get(session);
                    json!({
                        "session_id": session,
                        "run_id": step.map(|(run, _)| run),
                        "step_key": step.map(|(_, key)| key),
                        "rss_bytes": stats.rss_bytes,
                        "cpu_percent": stats.cpu_percent,
                        "uptime_s": stats.uptime_s,
                    })
                })
                .collect();
            sessions.sort_by_key(|s| std::cmp::Reverse(s["rss_bytes"].as_u64()));
            let devices: DeviceCapabilities = caps
                .get("devices")
                .and_then(|d| serde_json::from_value(d.clone()).ok())
                .unwrap_or_default();
            let bootable: Vec<String> = [&devices.ios_simulator, &devices.android_emulator]
                .into_iter()
                .filter(|tool| tool.ready)
                .flat_map(|tool| tool.devices.clone())
                .collect();
            let disk = state
                .disk
                .get(&host.id)
                .into_iter()
                .flatten()
                .filter(|(task, _)| !hidden_tasks.contains(task));
            json!({
                "id": host.id,
                "name": host.name,
                "workspace_ids": workspace_ids,
                "enrolled_by": host.enrolled_by,
                "status": if state.conns.contains_key(&host.id) { "online" } else { "offline" },
                "capabilities": {
                    "os": caps.get("os").cloned().unwrap_or(Value::from("")),
                    "arch": caps.get("arch").cloned().unwrap_or(Value::from("")),
                    "runtimes": adapters(caps).into_iter().map(|(a, ..)| a).collect::<Vec<_>>(),
                    // Bootable simulators/emulators of the ready tools.
                    "devices": bootable,
                    // `{ios_simulator, android_emulator, maestro}`, each
                    // `{ready, version, devices, detail}` (`detail` = the fix).
                    "device_tools": devices,
                    // Exec envs: `[{kind, ready, version, detail}]`; older hosts = bare only.
                    "exec_envs": caps.get("exec_envs").cloned()
                        .unwrap_or_else(|| json!([{ "kind": "bare", "ready": true, "version": null, "detail": null }])),
                },
                "sessions": sessions.len(),
                "session_stats": sessions,
                "disk": disk.map(|(task, bytes)| json!({ "task_id": task, "bytes": bytes })).collect::<Vec<_>>(),
                "last_seen_at": state.last_seen.get(&host.id).copied()
                    .unwrap_or_else(|| wire::created_at(&host.id.to_string())),
            })
        })
        .collect();
    Ok(Json(views))
}

/// When this kernel process started (first API router built).
static STARTED: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();

/// Marks the kernel's start for [`kernel`]'s uptime.
pub(crate) fn mark_started() {
    STARTED.get_or_init(Instant::now);
}

/// `GET /api/v1/kernel`: the kernel process's own resources, apart from any
/// agent session: `{pid, rss_bytes, cpu_percent, uptime_s}`. `cpu_percent`
/// is the lifetime average (CPU seconds over seconds alive, as `ps %cpu`).
/// `rss_bytes`/`cpu_percent` are `null` where neither `/proc` (Linux) nor
/// `ps` (elsewhere) can be read.
pub(crate) async fn kernel() -> Json<Value> {
    let pid = std::process::id();
    let usage = tokio::task::spawn_blocking(move || own_usage(pid))
        .await
        .ok()
        .flatten();
    Json(json!({
        "pid": pid,
        "rss_bytes": usage.map(|(rss, _)| rss),
        "cpu_percent": usage.map(|(_, cpu)| cpu),
        "uptime_s": STARTED.get().map_or(0, |at| at.elapsed().as_secs()),
    }))
}

/// `(rss bytes, cpu %)` of process `pid` alone (not its children: with
/// `--with-host` the sessions are children and count on the host), from
/// `/proc`, so the Alpine image needs no procps `ps`.
#[cfg(target_os = "linux")]
fn own_usage(pid: u32) -> Option<(u64, f32)> {
    use sun_core::procfs;
    let read = |path: &str| std::fs::read_to_string(path).ok();
    let uptime_s = procfs::parse_uptime(&read("/proc/uptime")?)?;
    let usage = procfs::usage(pid, uptime_s, read)?;
    Some((usage.rss_bytes, usage.cpu_percent))
}

/// `(rss bytes, cpu %)` of process `pid` alone (see the Linux version), via
/// `ps`, which on macOS is the only `unsafe`-free way to read it.
#[cfg(not(target_os = "linux"))]
fn own_usage(pid: u32) -> Option<(u64, f32)> {
    let output = std::process::Command::new("ps")
        .args(["-o", "rss=,%cpu=", "-p", &pid.to_string()])
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&output.stdout);
    let mut cols = text.split_whitespace();
    let rss_kb: u64 = cols.next()?.parse().ok()?;
    let cpu: f32 = cols.next()?.parse().ok()?;
    Some((rss_kb * 1024, cpu))
}

/// Body of `POST /sessions/{id}/kill`.
#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct KillInput {
    reason: String,
}

/// `POST /api/v1/sessions/{id}/kill {reason}` (admin): stops a live agent
/// session to free its host; its run pauses and records who and why.
/// `{run_id}` (`null` for a session no run owns).
pub(crate) async fn kill_session(
    State(api): State<Api>,
    axum::Extension(caller): axum::Extension<auth::Caller>,
    axum::extract::Path(session): axum::extract::Path<SessionId>,
    crate::routes::Body(input): crate::routes::Body<KillInput>,
) -> ApiResult<Json<Value>> {
    let reason = input.reason.trim();
    if reason.is_empty() || reason.len() > 500 {
        return Err(ApiError::invalid(
            "reason: say why in 1 to 500 characters; the run records it.",
        ));
    }
    let run = api
        .engine
        .kill_session(session, reason.to_owned(), caller.actor())
        .await?;
    Ok(Json(json!({ "run_id": run })))
}

/// `(adapter, installed, logged_in)` per runtime a host reported in its `hello`.
fn adapters(capabilities: &Value) -> Vec<(String, bool, bool)> {
    capabilities
        .get("runtimes")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|r| {
            let adapter = r.get("adapter")?.as_str()?.to_string();
            let readiness = r.get("readiness");
            let flag = |k: &str| readiness.and_then(|v| v.get(k)) == Some(&Value::Bool(true));
            Some((adapter, flag("installed"), flag("logged_in")))
        })
        .collect()
}

/// Where the runtime reads come from (`sun.yaml` `runtimes.model`, the
/// kill switch of the runtime model rework): `v1` parses each host's raw
/// capabilities report, `v2` reads the `host_runtime` rows projected from
/// it. Both see the same data; `v1` stays the default until `v2` has run
/// clean in production. `sun serve` also hands it to the engine's placement
/// (`sun_engine::RUNTIMES_MODEL`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RuntimesModel {
    /// Read `host.capabilities_json`.
    #[default]
    V1,
    /// Read `host_runtime`.
    V2,
}

/// `adapter -> (installed, installed and logged in)` on any online host
/// attached to `workspace` (`None`: any host), the ones its runs may use.
/// Read per [`RuntimesModel`].
pub(crate) async fn online_adapters(
    api: &Api,
    workspace: Option<WorkspaceId>,
) -> ApiResult<BTreeMap<String, (bool, bool)>> {
    let mut seen: BTreeMap<String, (bool, bool)> = BTreeMap::new();
    let reported: Vec<(HostId, String, bool, bool)> = match api.runtimes_model {
        RuntimesModel::V1 => api
            .engine
            .hosts(workspace)
            .await?
            .into_iter()
            .flat_map(|host| {
                adapters(&host.capabilities).into_iter().map(
                    move |(adapter, installed, logged_in)| (host.id, adapter, installed, logged_in),
                )
            })
            .collect(),
        RuntimesModel::V2 => api
            .engine
            .host_runtimes(workspace)
            .await?
            .into_iter()
            .map(|r| (r.host_id, r.tool, r.installed, r.auth == "logged_in"))
            .collect(),
    };
    for (host, adapter, installed, logged_in) in reported {
        let online = api.hosts.is_online(host);
        let entry = seen.entry(adapter).or_default();
        entry.0 |= online && installed;
        entry.1 |= online && installed && logged_in;
    }
    Ok(seen)
}

/// Query of `GET /runtimes`: `?workspace=` and `?model=v2` (the response
/// shape; the legacy `v1` shape is gone).
#[derive(serde::Deserialize)]
pub(crate) struct RuntimesQuery {
    workspace: Option<String>,
    model: Option<String>,
}

/// `GET /api/v1/runtimes?workspace=`: one entry per `(host, tool)` with an
/// honest `state` ([`crate::runtime_list`]). Only a deployment owner may
/// leave out `?workspace=` (every workspace's rows). Who pays is not a
/// runtime: see `GET /credentials`. `sun.yaml` `runtimes.model` picks the
/// data source (`v1` capabilities_json, `v2` host_runtime). `?model=v1`,
/// the legacy response shape, is `410`.
pub(crate) async fn list_runtimes(
    State(api): State<Api>,
    axum::Extension(caller): axum::Extension<auth::Caller>,
    axum::extract::Query(query): axum::extract::Query<RuntimesQuery>,
) -> ApiResult<Json<Vec<Value>>> {
    let workspace = listed_workspace(&caller, query.workspace.as_deref())?;
    match query.model.as_deref() {
        None | Some("v2") => Ok(Json(crate::runtime_list::list(&api, workspace).await?)),
        Some("v1") => Err(ApiError::new(
            StatusCode::GONE,
            "gone",
            "The v1 runtime list is gone. Read runtimes per host from this list without \
             `model`, and keys from GET /credentials.",
        )),
        Some(_) => Err(ApiError::invalid("model: v2.")),
    }
}

/// `POST /api/v1/runtimes` and `PUT /api/v1/runtimes/{id}`: `410`. Keys and
/// credits are credentials now (TRD BLO-117 item 7); a host reports its
/// runtimes on its own.
pub(crate) async fn runtime_write_gone() -> ApiResult<()> {
    Err(ApiError::new(
        StatusCode::GONE,
        "gone",
        "Runtimes can't be added or edited any more: a host reports its CLIs on its own. Add \
         an API key or credits with POST /credentials.",
    ))
}

/// `GET /api/v1/runtimes/{id}/readiness?workspace=` (`id` = a tool, e.g.
/// `claude-code`): see [`tool_readiness`]. A credential's readiness is in
/// `GET /credentials`.
pub(crate) async fn readiness(
    State(api): State<Api>,
    axum::Extension(caller): axum::Extension<auth::Caller>,
    axum::extract::Path(tool): axum::extract::Path<String>,
    axum::extract::Query(query): axum::extract::Query<InWorkspace>,
) -> ApiResult<Json<Readiness>> {
    let workspace = named_workspace(&caller, query.workspace.as_deref())?;
    let online = online_adapters(&api, Some(workspace)).await?;
    match online.contains_key(&tool) {
        true => Ok(Json(tool_readiness(&tool, &online))),
        false => Err(Error::NotFound(format!("runtime {tool}")).into()),
    }
}

/// The readiness of chain name `name` in `workspace`: the workspace's
/// credential of that name ([`credential_readiness`]), else the tool
/// ([`tool_readiness`]); `NotFound` for a name no credential or online
/// host knows.
pub(crate) async fn readiness_of(
    api: &Api,
    workspace: WorkspaceId,
    name: String,
    online: &BTreeMap<String, (bool, bool)>,
) -> ApiResult<Readiness> {
    match api.engine.credential(workspace, &name).await {
        Ok(credential) => credential_readiness(api, &credential, online).await,
        Err(sun_core::Error::NotFound(_)) if online.contains_key(&name) => {
            Ok(tool_readiness(&name, online))
        }
        Err(e) => Err(e.into()),
    }
}

/// A tool on the CLI's own login: `installed` = an online host reports it
/// installed; `logged_in` = such a host says it is logged in. `detail` says
/// what to fix.
pub(crate) fn tool_readiness(tool: &str, online: &BTreeMap<String, (bool, bool)>) -> Readiness {
    let (installed, logged_in) = online.get(tool).copied().unwrap_or_default();
    let detail = match (installed, logged_in) {
        (false, _) => Some(format!("no online host has {tool} installed")),
        (true, false) => Some(format!("log in to {tool} on a host")),
        (true, true) => None,
    };
    Readiness {
        installed,
        version: None,
        logged_in,
        detail,
    }
}

/// What the kernel can check of a credential without starting a session:
/// `installed` = an online host has its tool installed; `logged_in` = its
/// settings are valid and its key secret is stored (`byo_key`; the
/// endpoint itself is not called), or Sun Cloud is connected (`credits`).
/// `detail` says what to fix.
pub(crate) async fn credential_readiness(
    api: &Api,
    credential: &Credential,
    online: &BTreeMap<String, (bool, bool)>,
) -> ApiResult<Readiness> {
    let tool = &credential.default_tool;
    let (installed, _) = online.get(tool).copied().unwrap_or_default();
    let problem = match credential.source {
        RuntimeSource::Credits if !api.sun_cloud => {
            Some("Sun credits are not available on this deployment yet".into())
        }
        // The gateway key replaces the CLI's own login.
        RuntimeSource::Credits => None,
        _ => api.engine.credential_problem(credential).await?,
    };
    let detail = match (&problem, installed) {
        (Some(why), _) => Some(why.clone()),
        (None, false) => Some(format!("no online host has {tool} installed")),
        (None, true) => None,
    };
    Ok(Readiness {
        installed,
        version: None,
        logged_in: problem.is_none(),
        detail,
    })
}

/// `422` for a `bypass` the operator doesn't allow (`sun.yaml`
/// `runtimes.allow_bypass`): on an agent or a workflow step.
pub(crate) fn check_bypass_allowed(api: &Api, bypass: bool) -> ApiResult<()> {
    if bypass && !api.allow_bypass {
        return Err(ApiError::invalid(
            "bypass_permissions lets the agent run anything. Set `runtimes.allow_bypass: true` \
             in sun.yaml first, and only when sessions run in a container or VM.",
        ));
    }
    Ok(())
}

/// `PUT /api/v1/runtimes/{id}/policy`: `410`. `runtime_policy` is gone
/// (TRD BLO-117 item 14); each agent carries its own mode.
pub(crate) async fn runtime_policy_gone() -> ApiResult<()> {
    Err(ApiError::new(
        axum::http::StatusCode::GONE,
        "gone",
        "Runtime permission modes are gone. Set the permission mode on each agent.",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_depend_on_secret_and_host() {
        let host = HostId::generate();
        assert_eq!(host_key_for("s", host), host_key_for("s", host));
        assert_ne!(host_key_for("s", host), host_key_for("t", host));
        assert_ne!(
            host_key_for("s", host),
            host_key_for("s", HostId::generate())
        );
    }
}
