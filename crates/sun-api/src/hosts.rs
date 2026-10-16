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
    ExecOutput, ExecRequest, HOST_PROTOCOL_VERSION, HostFrame, KernelFrame, ReplyBody,
    ScreenshotRequest, SessionFile, SessionStats, Worktree,
};
use sun_core::model::{Runtime, RuntimeSource};
use sun_core::{
    BoxStream, ByoKey, Error, HostEvent, HostId, HostLink, MemberId, Readiness, Result, RunId,
    RuntimeId, SessionHandle, SessionId, SessionSpec, Steer, StepKey, StopMode, TaskId, Timestamp,
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
const FIRST_FRAME: Duration = Duration::from_secs(10);
/// Default wait for a reply.
pub(crate) const REPLY: Duration = Duration::from_secs(60);
/// `worktree.prepare` may clone a repo.
const PREPARE: Duration = Duration::from_secs(15 * 60);
/// A host is shown with a session while its stats are this fresh.
const STATS_FRESH: Duration = Duration::from_secs(15);
const TOKEN_TTL: Duration = Duration::from_secs(60 * 60);
const MAX_FRAME: usize = 16 * 1024 * 1024;
/// Buffered host events per `events()` subscriber before the reader waits.
const EVENT_BUFFER: usize = 1024;

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
    /// Bytes per task folder, from each host's latest `disk` frame.
    disk: HashMap<HostId, Vec<(TaskId, u64)>>,
    last_seen: HashMap<HostId, Timestamp>,
    subscribers: Vec<mpsc::Sender<HostEvent>>,
    /// Unused enrolment tokens: when they expire, and what they enrol.
    tokens: HashMap<String, (Instant, Enrols)>,
    generation: u64,
}

/// What an enrolment token enrols.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum Enrols {
    /// A host attached to the workspace, enrolled by who minted the token.
    Workspace(WorkspaceId, Option<MemberId>),
    /// A leased seller host serving only this marketplace task.
    Market(TaskId),
}

struct Conn {
    generation: u64,
    out: mpsc::UnboundedSender<KernelFrame>,
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

    /// A new single-use enrolment token for what it `enrols`, valid for an
    /// hour.
    pub(crate) fn issue_token(&self, enrols: Enrols) -> Result<String> {
        let token = random_hex()?;
        let mut state = self.state();
        let now = Instant::now();
        state.tokens.retain(|_, (expires, _)| *expires > now);
        state
            .tokens
            .insert(token.clone(), (now + TOKEN_TTL, enrols));
        Ok(token)
    }

    /// Consumes `token`: what it enrols, once, if it was issued and has not
    /// expired.
    fn take_token(&self, token: &str) -> Option<Enrols> {
        let mut state = self.state();
        let found = state
            .tokens
            .keys()
            .find(|t| auth::same(t.as_bytes(), token.as_bytes()))
            .cloned();
        found
            .and_then(|t| state.tokens.remove(&t))
            .filter(|(expires, _)| *expires > Instant::now())
            .map(|(_, enrols)| enrols)
    }

    fn attach(&self, host: HostId, out: mpsc::UnboundedSender<KernelFrame>) -> u64 {
        let mut state = self.state();
        state.generation += 1;
        let generation = state.generation;
        // The newest connection wins; dropping the old sender ends its writer.
        state.conns.insert(host, Conn { generation, out });
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

/// The next host frame; `None` on close, error or a frame that does not parse.
async fn next_frame(
    socket: &mut futures_util::stream::SplitStream<WebSocket>,
) -> Option<HostFrame> {
    while let Some(message) = socket.next().await {
        match message.ok()? {
            Message::Text(text) => return serde_json::from_str(text.as_str()).ok(),
            Message::Close(_) => return None,
            // Binary is ignored (protocol); ping/pong are answered by axum.
            _ => {}
        }
    }
    None
}

async fn connection(api: Api, socket: WebSocket) {
    let (mut sink, mut stream) = socket.split();
    let first = tokio::time::timeout(FIRST_FRAME, next_frame(&mut stream)).await;
    match first {
        Ok(Some(HostFrame::Enroll { token, name })) => {
            if let Some(frame) = enroll(&api, &token, &name).await {
                let _ = sink.send(text(&frame)).await;
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
            let known = api.engine.host(host_id).await.is_ok();
            let key = host_key_for(&api.config.host_secret, host_id);
            if major != HOST_PROTOCOL_VERSION.split('.').next()
                || !known
                || !auth::same(host_key.as_bytes(), key.as_bytes())
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
            let generation = hub.attach(host_id, out);
            // A host that joins (or comes back) may take queued work.
            api.queue.wake();
            let writer = tokio::spawn(async move {
                while let Some(frame) = outgoing.recv().await {
                    if sink.send(text(&frame)).await.is_err() {
                        return;
                    }
                }
                // Replaced by a newer connection of the same host.
                let _ = sink.close().await;
            });
            while let Some(frame) = next_frame(&mut stream).await {
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
                    HostFrame::Reply { id, body } => {
                        let waiting = hub.state().pending.remove(&id);
                        if let Some((_, _, reply)) = waiting {
                            let _ = reply.send(body);
                        }
                    }
                    // A second enroll/hello is a protocol violation.
                    HostFrame::Enroll { .. } | HostFrame::Hello { .. } => break,
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

/// Trades a valid token for a new host and its key; `None` closes silently.
async fn enroll(api: &Api, token: &str, name: &str) -> Option<KernelFrame> {
    let name = name.trim();
    if name.is_empty() || name.len() > 64 || name.chars().any(char::is_control) {
        return None;
    }
    let enrols = api.hosts.take_token(token)?;
    // A token minted before the plan filled up still can't pass it. A
    // leased seller host is paid capacity, not one of the plan's own hosts.
    let host = match enrols {
        Enrols::Workspace(workspace, by) => {
            let plan = api.entitlements.for_workspace(workspace).await;
            let name = name.to_string();
            api.engine
                .add_host(name, Some((workspace, &plan)), by)
                .await
                .ok()?
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
    let digest = Sha256::digest(format!("{secret}:{host}").as_bytes());
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
/// single-use token for `sun host join`; the host it enrols is attached to
/// the workspace. Hosts count against its plan (`limits.hosts`).
pub(crate) async fn create_token(
    State(api): State<Api>,
    axum::Extension(caller): axum::Extension<auth::Caller>,
    axum::extract::Path(workspace): axum::extract::Path<String>,
) -> ApiResult<(StatusCode, Json<Value>)> {
    let workspace = crate::routes::id(&workspace, "workspace")?;
    within_hosts(&api, workspace).await?;
    let token = api
        .hosts
        .issue_token(Enrols::Workspace(workspace, caller.member_id))?;
    let expires_at = chrono::Utc::now() + TOKEN_TTL;
    Ok((
        StatusCode::CREATED,
        Json(json!({ "token": token, "expires_at": expires_at })),
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
    api.queue.wake();
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

/// `adapter -> (installed, installed and logged in)` on any online host
/// attached to `workspace` (`None`: any host), the ones its runs may use.
pub(crate) async fn online_adapters(
    api: &Api,
    workspace: Option<WorkspaceId>,
) -> ApiResult<BTreeMap<String, (bool, bool)>> {
    let mut seen: BTreeMap<String, (bool, bool)> = BTreeMap::new();
    for host in api.engine.hosts(workspace).await? {
        let online = api.hosts.is_online(host.id);
        for (adapter, installed, logged_in) in adapters(&host.capabilities) {
            let entry = seen.entry(adapter).or_default();
            entry.0 |= online && installed;
            entry.1 |= online && installed && logged_in;
        }
    }
    Ok(seen)
}

/// `GET /api/v1/runtimes?workspace=`: one entry per adapter any host
/// reported (`ready` when an online host has it installed and logged in,
/// `workspace_id` null: every workspace's chains may name it), then the
/// workspace's stored `byo_key` runtimes with their `config` and `credits`
/// runtimes with their `model` (`ready` per [`readiness`]). Only a
/// deployment owner may leave out `?workspace=` (every workspace's rows).
// ponytail: byo_runtime entries are derived from host capabilities, not rows;
// cooldowns and session counts come with the runtime repo.
pub(crate) async fn list_runtimes(
    State(api): State<Api>,
    axum::Extension(caller): axum::Extension<auth::Caller>,
    axum::extract::Query(query): axum::extract::Query<InWorkspace>,
) -> ApiResult<Json<Vec<Value>>> {
    let workspace = listed_workspace(&caller, query.workspace.as_deref())?;
    let online = online_adapters(&api, workspace).await?;
    let modes: BTreeMap<String, String> =
        api.engine.permission_modes().await?.into_iter().collect();
    let view = |name: &str, adapter: &str, source: &str, ready: bool, config: Option<&ByoKey>| {
        json!({
            "model": null, "workspace_id": null,
            "id": name, "name": name, "adapter": adapter, "source": source,
            "status": if ready { "ready" } else { "offline" },
            "cooldown_until": null, "sessions": 0, "max_sessions": 0,
            "permission_mode": modes.get(adapter),
            "allow_bypass": api.allow_bypass,
            "config": config,
        })
    };
    let mut views: Vec<Value> = online
        .iter()
        .map(|(adapter, (_, ok))| view(adapter, adapter, "byo_runtime", *ok, None))
        .collect();
    for runtime in api.engine.runtimes(workspace).await? {
        if runtime.source == RuntimeSource::ByoRuntime {
            continue;
        }
        let ready = runtime_readiness(&api, &runtime, &online).await?;
        let ok = ready.installed && ready.logged_in;
        let source = runtime.source.as_str();
        let config = runtime.byo_key.as_ref();
        let mut entry = view(&runtime.name, &runtime.adapter, source, ok, config);
        entry["workspace_id"] = json!(runtime.workspace_id);
        if runtime.source == RuntimeSource::Credits {
            entry["model"] = json!(api.engine.credits_model(runtime.id).await?);
        }
        views.push(entry);
    }
    Ok(Json(views))
}

/// Body of `POST /runtimes` and `PUT /runtimes/{id}`. `config` is
/// `{endpoint, model, key_secret}` (`byo_key`); a raw key is refused
/// (unknown field). `model` is the gateway model of a `credits` OpenCode
/// runtime.
#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RuntimeInput {
    #[serde(default)]
    name: String,
    #[serde(default)]
    adapter: Option<String>,
    source: RuntimeSource,
    #[serde(default)]
    config: Option<ByoKey>,
    #[serde(default)]
    model: Option<String>,
}

/// The deployment sells Sun credits. Off for the MVP: no new `credits`
/// runtime is accepted, even with Sun Cloud connected.
// ponytail: a constant until credits are sold; then the `sun_cloud` gate alone.
const CREDITS_ON_SALE: bool = false;

/// A configured runtime of `caller`'s for `workspace` from `input`, checked
/// by [`Runtime::validate`]. The 422 message starts with the field to fix
/// (`name: `, `endpoint: `, ...).
fn runtime_of(
    api: &Api,
    caller: &auth::Caller,
    workspace: WorkspaceId,
    name: String,
    input: &mut RuntimeInput,
) -> ApiResult<Runtime> {
    match input.source {
        RuntimeSource::ByoKey => {}
        RuntimeSource::Credits if CREDITS_ON_SALE && api.sun_cloud => {}
        RuntimeSource::Credits => {
            return Err(ApiError::invalid(
                "source: Sun credits are not available on this deployment yet; bring your own \
                 key or runtime.",
            ));
        }
        RuntimeSource::ByoRuntime => {
            return Err(ApiError::invalid(
                "source: BYO runtimes appear when a host reports the CLI.",
            ));
        }
    }
    let runtime = Runtime {
        id: RuntimeId::generate(),
        workspace_id: workspace,
        name,
        adapter: input.adapter.take().unwrap_or_else(|| "opencode".into()),
        source: input.source,
        byo_key: input.config.take(),
        created_by: caller.member_id,
    };
    match runtime.validate() {
        Ok(()) => Ok(runtime),
        Err(sun_core::Error::InvalidInput(why)) => Err(ApiError::invalid(why)),
        Err(e) => Err(e.into()),
    }
}

/// `POST /api/v1/runtimes?workspace=` (`manage_runtimes` there): adds a
/// `byo_key` runtime to the workspace, or with Sun Cloud connected a
/// `credits` one; `409` when the workspace has the name. Audited.
pub(crate) async fn create_runtime(
    State(api): State<Api>,
    axum::Extension(caller): axum::Extension<auth::Caller>,
    axum::extract::Query(query): axum::extract::Query<InWorkspace>,
    crate::routes::Body(mut input): crate::routes::Body<RuntimeInput>,
) -> ApiResult<Json<Value>> {
    let workspace = named_workspace(&caller, query.workspace.as_deref())?;
    let name = std::mem::take(&mut input.name);
    let runtime = runtime_of(&api, &caller, workspace, name, &mut input)?;
    let plan = api.entitlements.for_workspace(workspace).await;
    if !api
        .engine
        .save_runtime(
            &runtime,
            input.model.as_deref(),
            false,
            &caller.actor(),
            &plan,
        )
        .await?
    {
        return Err(ApiError::conflict(format!(
            "name: a runtime named `{}` already exists.",
            runtime.name
        )));
    }
    Ok(Json(saved_view(&runtime, input.model.as_deref())))
}

/// What `POST`/`PUT /runtimes` answer: the stored fields of a `GET` entry.
fn saved_view(r: &Runtime, model: Option<&str>) -> Value {
    let model = model.map(str::trim).filter(|m| !m.is_empty());
    json!({
        "id": r.name, "name": r.name, "workspace_id": r.workspace_id, "adapter": r.adapter,
        "source": r.source, "config": r.byo_key, "model": model,
    })
}

/// `PUT /api/v1/runtimes/{id}?workspace=` (`manage_runtimes` there, `id` =
/// the runtime's name): replaces its source config. Audited.
pub(crate) async fn update_runtime(
    State(api): State<Api>,
    axum::Extension(caller): axum::Extension<auth::Caller>,
    axum::extract::Path(name): axum::extract::Path<String>,
    axum::extract::Query(query): axum::extract::Query<InWorkspace>,
    crate::routes::Body(mut input): crate::routes::Body<RuntimeInput>,
) -> ApiResult<Json<Value>> {
    if !input.name.is_empty() && input.name != name {
        return Err(ApiError::invalid("name: a runtime can't be renamed."));
    }
    let workspace = named_workspace(&caller, query.workspace.as_deref())?;
    let runtime = runtime_of(&api, &caller, workspace, name, &mut input)?;
    let plan = api.entitlements.for_workspace(workspace).await;
    if !api
        .engine
        .save_runtime(
            &runtime,
            input.model.as_deref(),
            true,
            &caller.actor(),
            &plan,
        )
        .await?
    {
        return Err(sun_core::Error::NotFound(format!("runtime {}", runtime.name)).into());
    }
    Ok(Json(saved_view(&runtime, input.model.as_deref())))
}

/// `GET /api/v1/runtimes/{id}/readiness?workspace=` (`id` = a runtime's
/// name in that workspace): see [`runtime_readiness`].
pub(crate) async fn readiness(
    State(api): State<Api>,
    axum::Extension(caller): axum::Extension<auth::Caller>,
    axum::extract::Path(name): axum::extract::Path<String>,
    axum::extract::Query(query): axum::extract::Query<InWorkspace>,
) -> ApiResult<Json<Readiness>> {
    let workspace = named_workspace(&caller, query.workspace.as_deref())?;
    let online = online_adapters(&api, Some(workspace)).await?;
    Ok(Json(readiness_of(&api, workspace, name, &online).await?))
}

/// The readiness of `workspace`'s runtime named `name` (see
/// [`runtime_readiness`]); `NotFound` for a name no row or host knows.
pub(crate) async fn readiness_of(
    api: &Api,
    workspace: WorkspaceId,
    name: String,
    online: &BTreeMap<String, (bool, bool)>,
) -> ApiResult<Readiness> {
    let runtime = match api.engine.runtime(workspace, &name).await {
        Ok(runtime) => runtime,
        // A runtime only known from host capabilities (byo_runtime).
        Err(sun_core::Error::NotFound(_)) if online.contains_key(&name) => Runtime {
            id: RuntimeId::generate(),
            workspace_id: workspace,
            name: name.clone(),
            adapter: name,
            source: RuntimeSource::ByoRuntime,
            byo_key: None,
            created_by: None,
        },
        Err(e) => return Err(e.into()),
    };
    runtime_readiness(api, &runtime, online).await
}

/// What the kernel can check without starting a session: `installed` = an
/// online host reports the adapter installed; `logged_in` = the host says it
/// is logged in (byo_runtime), or the config is valid and its key secret is
/// stored (byo_key; the endpoint itself is not called). `detail` says what
/// to fix.
async fn runtime_readiness(
    api: &Api,
    runtime: &Runtime,
    online: &BTreeMap<String, (bool, bool)>,
) -> ApiResult<Readiness> {
    let (installed, host_logged_in) = online.get(&runtime.adapter).copied().unwrap_or_default();
    let problem = match runtime.source {
        RuntimeSource::ByoKey => api.engine.runtime_problem(runtime).await?,
        // The gateway key replaces the CLI's own login.
        RuntimeSource::Credits if api.sun_cloud => None,
        RuntimeSource::Credits => {
            Some("Sun credits are not available on this deployment yet".into())
        }
        _ if host_logged_in => None,
        _ => Some(format!("log in to {} on a host", runtime.adapter)),
    };
    let detail = match (&problem, installed) {
        (Some(why), _) => Some(why.clone()),
        (None, false) => Some(format!("no online host has {} installed", runtime.adapter)),
        (None, true) => None,
    };
    Ok(Readiness {
        installed,
        version: None,
        logged_in: problem.is_none(),
        detail,
    })
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SetPolicy {
    permission_mode: String,
}

/// `PUT /api/v1/runtimes/{id}/policy {permission_mode}` (admin): the mode
/// every new session of that runtime gets, on any host. Audited.
pub(crate) async fn set_runtime_policy(
    State(api): State<Api>,
    axum::Extension(caller): axum::Extension<auth::Caller>,
    axum::extract::Path(adapter): axum::extract::Path<String>,
    crate::routes::Body(input): crate::routes::Body<SetPolicy>,
) -> ApiResult<Json<Value>> {
    let valid_id = |c: char| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.');
    if adapter.is_empty() || adapter.len() > 64 || !adapter.chars().all(valid_id) {
        return Err(ApiError::invalid(format!("`{adapter}` is not a runtime.")));
    }
    let mode = input.permission_mode;
    if !sun_core::PERMISSION_MODES.contains(&mode.as_str()) {
        return Err(ApiError::invalid(format!(
            "`{mode}` is not a permission mode. Use {}.",
            sun_core::PERMISSION_MODES.join(", ")
        )));
    }
    if mode == "bypass_permissions" && !api.allow_bypass {
        return Err(ApiError::invalid(
            "bypass_permissions lets the agent run anything. Set `runtimes.allow_bypass: true` \
             in sun.yaml first, and only when sessions run in a container or VM.",
        ));
    }
    api.engine
        .set_permission_mode(&adapter, &mode, &caller.actor())
        .await?;
    Ok(Json(json!({ "id": adapter, "permission_mode": mode })))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokens_are_single_use_and_keys_depend_on_secret_and_host() {
        let hub = HostHub::default();
        let ops = Enrols::Workspace(WorkspaceId::generate(), None);
        let token = hub.issue_token(ops).unwrap();
        assert_eq!(token.len(), 64);
        assert_eq!(hub.take_token("nope"), None);
        assert_eq!(hub.take_token(&token), Some(ops));
        assert_eq!(hub.take_token(&token), None);
        let task = TaskId::generate();
        let leased = hub.issue_token(Enrols::Market(task)).unwrap();
        assert_eq!(hub.take_token(&leased), Some(Enrols::Market(task)));
        let host = HostId::generate();
        assert_eq!(host_key_for("s", host), host_key_for("s", host));
        assert_ne!(host_key_for("s", host), host_key_for("t", host));
        assert_ne!(
            host_key_for("s", host),
            host_key_for("s", HostId::generate())
        );
    }
}
