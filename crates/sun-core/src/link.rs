//! Boundary traits the engine and the `sun` MCP server use instead of concrete
//! crates. `sun-cli` injects the real implementations.

use std::path::PathBuf;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::host_protocol::{
    DeviceFlowRequest, DeviceInput, DeviceRequest, DeviceResult, DeviceScreen, ExecOutput,
    ExecRequest, ScreenshotRequest, SessionFile, SessionStats, Worktree,
};
use crate::model::{Mail, ModInstall, ModKind, Note, NoteKind, SmtpSettings};
use crate::{
    BoxStream, Error, HostId, IdemKey, NoteId, Result, RunId, RunStatus, SessionHandle, SessionId,
    SessionSpec, Steer, StepKey, StopMode, SunEvent, TaskId,
};

/// The engine's view of the runtime hosts.
///
/// Contract:
/// - [`events`](Self::events) yields host events with `(session_id, seq)`;
///   the same pair may be delivered more than once (after reconnects), so the
///   consumer stores them under a unique constraint and then calls
///   [`ack`](Self::ack) with the highest contiguous stored `seq`.
/// - [`is_alive`](Self::is_alive) answers the resume-on-boot question (the
///   host's `probe` frame); `Err`
///   means the host itself is unreachable.
/// - [`stop_session`](Self::stop_session) is idempotent.
#[async_trait]
pub trait HostLink: Send + Sync {
    /// Starts a session of runtime `adapter` (a `RuntimeAdapter::id`) on `host`.
    async fn start_session(
        &self,
        host: HostId,
        adapter: &str,
        spec: SessionSpec,
    ) -> Result<SessionHandle>;

    /// Steers a running session.
    async fn steer(&self, session: &SessionHandle, msg: Steer) -> Result<()>;

    /// Stops a session.
    async fn stop_session(&self, session: &SessionHandle, how: StopMode) -> Result<()>;

    /// Whether the session is still running on its host. `NotFound` when no
    /// connected host has it (its host may be offline, or it is gone);
    /// `Unavailable` when its host can't be asked right now.
    async fn is_alive(&self, session: &SessionHandle) -> Result<bool>;

    /// Events from all hosts, merged.
    fn events(&self) -> BoxStream<'static, HostEvent>;

    /// Confirms that every event of `session` up to and including `seq` is stored.
    async fn ack(&self, session: SessionId, seq: u64) -> Result<()>;

    /// Whether `host` is connected now (placement skips offline hosts).
    /// Default: `true` (single-host links and fakes).
    fn is_online(&self, host: HostId) -> bool {
        let _ = host;
        true
    }

    /// Makes sure the task's working directory exists on `host` (a git worktree
    /// on branch `sun/<task_id>`, or a task folder) and returns its host path,
    /// to use as [`SessionSpec::worktree`] and [`ExecRequest::cwd`].
    /// Idempotent. Default: `Unavailable`.
    async fn prepare_worktree(&self, host: HostId, worktree: Worktree) -> Result<PathBuf> {
        let _ = worktree;
        Err(Error::Unavailable(format!(
            "host {host}: worktrees not supported"
        )))
    }

    /// Removes the task's working directory on `host` (all its worktrees;
    /// branches stay in the bare clones). Idempotent. Default: `Unavailable`.
    async fn remove_worktree(&self, host: HostId, task_id: TaskId) -> Result<()> {
        let _ = task_id;
        Err(Error::Unavailable(format!(
            "host {host}: removing worktrees not supported"
        )))
    }

    /// Writes a session's `.sun/` files (context pack, hand-off, progress)
    /// into a prepared worktree on `host`, clearing `.sun/context/` first.
    /// Files with `overwrite: false` are only created when missing. Every
    /// path must be relative and under `.sun/`. Default: `Unavailable`.
    async fn write_files(&self, host: HostId, cwd: PathBuf, files: Vec<SessionFile>) -> Result<()> {
        let _ = (cwd, files);
        Err(Error::Unavailable(format!(
            "host {host}: writing files not supported"
        )))
    }

    /// Runs a shell command in a prepared worktree on `host` and returns its
    /// exit code and output (gates). Default: `Unavailable`.
    async fn exec(&self, host: HostId, request: ExecRequest) -> Result<ExecOutput> {
        let _ = request;
        Err(Error::Unavailable(format!(
            "host {host}: exec not supported"
        )))
    }

    /// Builds the app, boots a fresh simulator or emulator on `host`, installs
    /// the app, runs the Maestro flow and returns screenshots. A flow
    /// that fails is `Ok` with `passed: false`; `Unavailable` means the host
    /// lacks the tools. Default: `Unavailable`.
    async fn verify_on_device(&self, host: HostId, request: DeviceRequest) -> Result<DeviceResult> {
        let _ = request;
        Err(Error::Unavailable(format!(
            "host {host}: device verify not supported"
        )))
    }

    /// One picture of the screen of `task_id`'s device on `host`.
    /// `NotFound` while none is up. Default: `Unavailable`.
    async fn device_screen(&self, host: HostId, task_id: TaskId) -> Result<DeviceScreen> {
        let _ = task_id;
        Err(Error::Unavailable(format!(
            "host {host}: device view not supported"
        )))
    }

    /// A tap, text or key on `task_id`'s device. `NotFound` while none is
    /// up. Default: `Unavailable`.
    async fn device_input(&self, host: HostId, task_id: TaskId, input: DeviceInput) -> Result<()> {
        let _ = (task_id, input);
        Err(Error::Unavailable(format!(
            "host {host}: device input not supported"
        )))
    }

    /// Runs a Maestro flow on the task's device, booting one held for the
    /// task when none is up (an agent driving it during its step). A failing
    /// flow is `Ok` with `passed: false`. Default: `Unavailable`.
    async fn device_flow(&self, host: HostId, request: DeviceFlowRequest) -> Result<DeviceResult> {
        let _ = request;
        Err(Error::Unavailable(format!(
            "host {host}: device flows not supported"
        )))
    }

    /// A PNG of a page of the task's preview, from a headless browser
    /// on `host`; starts the side's dev server when none runs.
    /// Default: `Unavailable`.
    async fn preview_screenshot(
        &self,
        host: HostId,
        request: ScreenshotRequest,
    ) -> Result<Vec<u8>> {
        let _ = request;
        Err(Error::Unavailable(format!(
            "host {host}: preview screenshots not supported"
        )))
    }

    /// The latest resource sample of a live session (its host sends one every
    /// 5 s). `NotFound` before the first sample. Default: `Unavailable`.
    async fn stats(&self, session: SessionId) -> Result<SessionStats> {
        Err(Error::Unavailable(format!(
            "session {session}: stats not supported"
        )))
    }
}

/// An event from a host session, numbered for exactly-once storage.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HostEvent {
    /// The session that produced it.
    pub session_id: SessionId,
    /// Per-session sequence number, starting at 1, gapless.
    pub seq: u64,
    /// The event.
    pub event: SunEvent,
}

/// The engine's view of installed mods (**Plugin** over JSON-RPC stdio).
///
/// Contract: a call carrying an [`IdemKey`] may be delivered more than once;
/// the mod must make it idempotent. A timeout returns
/// [`Error::Unavailable`](crate::Error::Unavailable) and the caller marks the
/// outbox row `unknown` instead of retrying blindly.
#[async_trait]
pub trait ModLink: Send + Sync {
    /// Calls `call.method` on mod `call.mod_name` and returns its JSON result.
    async fn call(&self, call: ModCall) -> Result<serde_json::Value>;

    /// Adds `triggers` to the set mod `mod_name` should emit (callers
    /// subscribe independently; the set is their union and survives mod
    /// restarts). An undeclared trigger fails the call. The events arrive on
    /// [`events`](Self::events). Default: `Unavailable` (no trigger support).
    async fn subscribe(&self, mod_name: &str, triggers: Vec<String>) -> Result<()> {
        let _ = triggers;
        Err(Error::Unavailable(format!(
            "{mod_name}: triggers not supported"
        )))
    }

    /// Events from all mods, merged. Every call returns an independent stream
    /// that sees events emitted after it was created. Default: empty.
    fn events(&self) -> BoxStream<'static, ModEvent> {
        Box::pin(Empty)
    }
}

/// Reserved [`ModLink::call`] methods of an extension with
/// `provides.runtime_adapters` (mod API 1.2), the kernel's side of
/// `sun_mod_sdk::runtime_action` (the standalone SDK keeps its own copy;
/// `sun-mod` tests that they match).
pub mod runtime_action {
    /// `{adapter, host_id}` → `Readiness`.
    pub const READINESS: &str = "runtime.readiness";
    /// `{adapter, spec}` (a `SessionSpec` without `env`) → `{native_id?}`.
    pub const START: &str = "runtime.start";
    /// `{adapter, session_id, text}` → `null`.
    pub const SEND: &str = "runtime.send";
    /// `{adapter, session_id, how}` → `null`; idempotent.
    pub const STOP: &str = "runtime.stop";
    /// `{adapter, session_id, after}` → `{events}` from index `after` on.
    pub const EVENTS: &str = "runtime.events";
    /// Every runtime action.
    pub const ALL: [&str; 5] = [READINESS, START, SEND, STOP, EVENTS];
}

/// Reserved [`ModLink::call`] methods of an extension with
/// `provides.exec_envs` (mod API 1.2), the kernel's side of
/// `sun_mod_sdk::exec_env_action`.
pub mod exec_env_action {
    /// `{kind, session_id, worktree, env_names, config}` → `{env?}`.
    pub const PREPARE: &str = "exec_env.prepare";
    /// `{kind, session_id, cwd, command, env, timeout_s}` → `ExecOutput`.
    pub const EXEC: &str = "exec_env.exec";
    /// `{kind, session_id}` → `null`; idempotent.
    pub const TEARDOWN: &str = "exec_env.teardown";
    /// Every exec env action.
    pub const ALL: [&str; 3] = [PREPARE, EXEC, TEARDOWN];
}

/// An event a mod pushed to the kernel: a fired trigger.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModEvent {
    /// Installed mod name.
    pub mod_name: String,
    /// Trigger name from the mod's manifest, e.g. `issue_labeled`.
    pub topic: String,
    /// Trigger data.
    pub payload: serde_json::Value,
}

/// A stream that ends immediately.
struct Empty;

impl futures_core::Stream for Empty {
    type Item = ModEvent;

    fn poll_next(
        self: std::pin::Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<ModEvent>> {
        std::task::Poll::Ready(None)
    }
}

/// One mod method call.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModCall {
    /// Installed mod name, e.g. `github`.
    pub mod_name: String,
    /// Method, e.g. `open_pr`.
    pub method: String,
    /// JSON-RPC params.
    pub params: serde_json::Value,
    /// Set for side effects delivered from the outbox.
    pub idem_key: Option<IdemKey>,
}

/// A mod package that can be installed: listed in the embedded mod
/// registry (`catalog/mods.json`), installed from its GitHub Release.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AvailableMod {
    /// Mod name.
    pub name: String,
    /// Plugin or extension.
    pub kind: ModKind,
    /// The version an install gets.
    pub version: String,
    /// What it asks for (`kind:value`, as `sun_mod::permission_list`).
    pub permissions: Vec<String>,
    /// Signed by its publisher (known only once downloaded: `false` here).
    pub signed: bool,
    /// What it does, one sentence.
    pub description: String,
    /// Its source repo, `https://github.com/<owner>/<repo>`.
    pub repo: String,
    /// `official` (Madlabs) or `community` (not reviewed by Madlabs).
    pub tier: String,
}

/// Installs mods from inside the running kernel (`POST /mods/{name}/install`).
///
/// Contract: [`install`](Self::install) installs only what
/// [`available`](Self::available) lists (the mods in `catalog/mods.json`),
/// and only when `granted` equals the listed permissions (explicit consent;
/// otherwise `InvalidInput`). It downloads the listed version's release
/// asset for this platform, checks it is that mod and version (and its
/// signature, when signed), writes the `mod_install` row and its
/// `mod_install` audit entry by `actor` in one transaction, and loads the mod
/// into the running mod host. Unknown names are `NotFound`; a listed mod with
/// no asset for this platform is `Unavailable`.
#[async_trait]
pub trait ModInstaller: Send + Sync {
    /// Packages that can be installed, installed ones included.
    async fn available(&self) -> Result<Vec<AvailableMod>>;

    /// Installs (or updates) mod `name` with the `granted` permissions.
    async fn install(&self, name: &str, granted: &[String], actor: &str) -> Result<ModInstall>;

    /// Stops and uninstalls mod `name`, audited as `actor`; its data and
    /// secrets go too only when `purge`. Returns the installed extensions
    /// that declare it in `permissions.plugins`. Not installed: `NotFound`.
    async fn remove(&self, name: &str, purge: bool, actor: &str) -> Result<Vec<String>>;
}

/// A buyer's request for marketplace runtime capacity,
/// the wire form of Sun Cloud's `POST /marketplace/v1/leases`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LeaseRequest {
    /// The buyer's task id.
    pub task_ref: String,
    /// Runtime adapter wanted, e.g. `claude-code`.
    pub runtime: String,
    /// `hour` or `task`.
    pub unit: String,
    /// Most the buyer pays per unit, in points.
    pub max_price_points: i64,
    /// Only this model.
    pub model: Option<String>,
    /// Only this region.
    pub region: Option<String>,
    /// The buyer kernel's public URL the seller's host joins.
    pub kernel_url: String,
    /// Single-use host join token, bound to the task.
    pub join_token: String,
    /// Only listings with this capability (`gpu`, `ios_simulator`, `kvm`, ...).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capability: Option<String>,
    /// Only listings whose average rating is at least this (0–5).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_rating: Option<f64>,
}

/// What the catalog of runtime listings is filtered by; every field optional
/// (Sun Cloud's `GET /marketplace/v1/listings` query).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ListingFilter {
    /// Runtime adapter, e.g. `claude-code`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runtime: Option<String>,
    /// A region (`id-jkt`) or a country prefix (`id`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub region: Option<String>,
    /// `machine`, `open_model`, `api` or `subscription`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub offering: Option<String>,
    /// A capability the listing must have.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capability: Option<String>,
    /// Lowest average rating (unrated listings are left out).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_rating: Option<f64>,
}

/// The kernel's account on the Sun Cloud marketplace, as a buyer and a
/// seller.
///
/// Contract: every call is made as this kernel's ecosystem; leases,
/// listings and payouts come back in Sun Cloud's JSON form (passed through to
/// the UI). Marketplace refusals map to `InvalidInput` (400/409), `Forbidden`
/// (401/402/403) and `NotFound` (404), with Sun Cloud's message; an
/// unreachable cloud is `Unavailable`. Credentials never appear in errors.
#[async_trait]
pub trait Marketplace: Send + Sync {
    /// Active runtime listings `filter` keeps (each with the disclosed model,
    /// any warning, its capabilities and rating).
    async fn listings(&self, filter: &ListingFilter) -> Result<serde_json::Value>;
    /// This ecosystem's own listings, active and paused.
    async fn my_listings(&self) -> Result<serde_json::Value>;
    /// Pauses (`paused`) or resumes (`active`) one of this ecosystem's listings.
    async fn set_listing_status(&self, listing_id: &str, status: &str)
    -> Result<serde_json::Value>;
    /// `{payable, payouts}`: earned points not paid out yet, and past payouts.
    async fn payouts(&self) -> Result<serde_json::Value>;
    /// Pays `points` of the earnings out in `currency` (`usd`, `idr`) to
    /// `destination` (a provider payee reference).
    async fn request_payout(
        &self,
        points: i64,
        currency: &str,
        destination: &str,
    ) -> Result<serde_json::Value>;
    /// Rates a closed lease 1–5 stars, once, with an optional comment.
    async fn rate_lease(
        &self,
        lease_id: &str,
        stars: i64,
        comment: Option<&str>,
    ) -> Result<serde_json::Value>;
    /// Leases this ecosystem is party to, newest first.
    async fn leases(&self) -> Result<serde_json::Value>;
    /// Leases the cheapest admitted listing for `request`.
    async fn lease(&self, request: &LeaseRequest) -> Result<serde_json::Value>;
    /// Ends an open lease (escrow stays held for the dispute window).
    async fn close(&self, lease_id: &str) -> Result<serde_json::Value>;
    /// Accepts the work: the escrow goes to the seller now.
    async fn accept(&self, lease_id: &str) -> Result<serde_json::Value>;
    /// Disputes the work: the points stay held until Sun decides.
    async fn dispute(&self, lease_id: &str, reason: &str) -> Result<serde_json::Value>;
    /// Ends an open lease no work ran on (a mid-run hand-off that never
    /// reached the seller): the escrow is refunded. Idempotent: a closed
    /// lease comes back unchanged.
    async fn release(&self, lease_id: &str) -> Result<serde_json::Value>;
}

/// What the `sun` MCP server (in `sun-context`) may ask of the engine.
///
/// Contract: every call is attributed to the run/step that made it and lands
/// in the event log; nothing here can move a step to `succeeded`. Callers
/// scope every call to their own run (a session token sees one task); text
/// arriving from agents is already redacted.
#[async_trait]
pub trait EngineHandle: Send + Sync {
    /// Records a note from an agent (`sun.note`).
    async fn add_note(
        &self,
        run_id: RunId,
        step_key: StepKey,
        kind: NoteKind,
        body: String,
    ) -> Result<NoteId>;

    /// Current status of a run.
    async fn run_status(&self, run_id: RunId) -> Result<RunStatus>;

    /// The task and step brief (`sun.task`). `NotFound` if the step is not in the run.
    async fn task(&self, run_id: RunId, step_key: StepKey) -> Result<StepBrief>;

    /// Resolves a pointer from a context pack (`sun.get`): a blob, an earlier
    /// artifact, a doc or a transcript chunk, as text. Only pointers that
    /// belong to `run_id` resolve; anything else is `NotFound`.
    async fn get(&self, run_id: RunId, pointer: String) -> Result<String>;

    /// Notes of the run, including parallel siblings' (`sun.notes`, the
    /// blackboard), oldest first.
    async fn notes(&self, run_id: RunId) -> Result<Vec<Note>>;

    /// Asks a human (`sun.ask`) and waits for the answer. `options`, when not
    /// empty, are the allowed answers.
    async fn ask(
        &self,
        run_id: RunId,
        step_key: StepKey,
        question: String,
        options: Vec<String>,
    ) -> Result<String>;

    /// The agent claims the step is done (`sun.done`). Requests verification:
    /// the step may move to `verifying`, never to `succeeded`.
    async fn done(&self, run_id: RunId, step_key: StepKey, summary: String) -> Result<()>;

    /// Searches the knowledge the run's task selected (`sun.search`), hybrid
    /// when embeddings are on; the hits rendered for the agent, with
    /// citations. Default `Unavailable`: the MCP server then falls back to
    /// its own BM25 search over every source.
    async fn search(&self, run_id: RunId, query: String, k: usize) -> Result<String> {
        let _ = (run_id, query, k);
        Err(crate::Error::Unavailable(
            "knowledge search is not enabled".into(),
        ))
    }

    /// The screen of the run's task device (`sun.device_screen`).
    /// `NotFound` while none is up. Default `Unavailable`.
    async fn device_screen(&self, run_id: RunId) -> Result<DeviceScreen> {
        let _ = run_id;
        Err(Error::Unavailable("devices are not enabled".into()))
    }

    /// A tap, text or key on the run's task device (`sun.device_tap`).
    /// Default `Unavailable`.
    async fn device_input(
        &self,
        run_id: RunId,
        step_key: StepKey,
        input: DeviceInput,
    ) -> Result<()> {
        let _ = (run_id, step_key, input);
        Err(Error::Unavailable("devices are not enabled".into()))
    }

    /// Runs a Maestro flow on the run's task device, booting one when asked
    /// (`sun.device_flow`). `request.cwd` is ignored: the engine resolves the
    /// task's worktree. Default `Unavailable`.
    async fn device_flow(
        &self,
        run_id: RunId,
        step_key: StepKey,
        request: DeviceFlowRequest,
    ) -> Result<DeviceResult> {
        let _ = (run_id, step_key, request);
        Err(Error::Unavailable("devices are not enabled".into()))
    }
}

/// What an agent step is asked to do: the P0 items of a context pack
/// and the answer to `sun.task`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StepBrief {
    /// The task.
    pub task_id: TaskId,
    /// Task title.
    pub title: String,
    /// Task description.
    pub body: String,
    /// The workflow step.
    pub step_key: StepKey,
    /// What this step must achieve.
    pub goal: String,
    /// Pinned constraints the agent must respect.
    pub constraints: Vec<String>,
    /// Gates the step must pass, e.g. `test`.
    pub gates: Vec<String>,
    /// Acceptance criteria.
    pub acceptance: Vec<String>,
    /// The step's `context.budget_tokens`; `None` uses 10% of the window.
    pub context_budget_tokens: Option<u32>,
}

/// Sends mail (invites, the SMTP test). **Adapter** over an SMTP client;
/// `sun-cli` injects the real one, tests a fake that records mails.
///
/// Contract: settings arrive per call (an admin may change them at any
/// time); `password` is `None` when no SMTP password is stored. An error
/// message never contains the password or the mail body.
#[async_trait]
pub trait Mailer: Send + Sync {
    /// Sends `mail` through `smtp`.
    async fn send(&self, smtp: &SmtpSettings, password: Option<&str>, mail: &Mail) -> Result<()>;
}
