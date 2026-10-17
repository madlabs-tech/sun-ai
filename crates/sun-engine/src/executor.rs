//! The durable executor.
//!
//! **Pattern:** a single-task **state machine** driver (one writer, no races)
//! over the step and run machines in `sun_core`, with a **Transactional
//! outbox** for side effects and the event log as the journal.
//!
//! Rules every handler follows:
//! 1. Open one write transaction, read the rows, move states through
//!    `set_*_status` (which applies `StateMachine::transition`), append the
//!    events, enqueue outbox rows, commit. Nothing else changes state.
//! 2. Only after commit, run the [`Effect`]s (host calls, timers, outbox
//!    wake-up). No transaction is open during host or mod I/O.
//! 3. Every effect can be re-derived from the database, so a crash between
//!    commit and effect is repaired by [`Executor::resume`] on the next boot.
//!
//! ## Extension points (for the next phases)
//! - **Queue:** `crate::queue` creates `queued` runs and calls
//!   [`Engine::start_run`]; `move_run` moves the task with its run.
//! - **Step drivers:** [`Executor::start_step`] has one exhaustive `match` arm
//!   per step type. A new type fills its arm;
//!   [`Executor::finish`] is where the engine ends a step.
//! - **Gates:** agent steps go `verifying → succeeded` straight away in
//!   [`Executor::on_host_event`]; the `gate` steps that need them verify the
//!   work and send it back (`gates`). Approvals: `approval`.
//! - **Watchdog:** `watchdog` (a child module) watches running agent
//!   steps through three hooks: `watch` when a step starts running, `police`
//!   on each stored agent event, and [`Command::Watch`] for its deadlines.
//! - **Failover:** `failover` (a child module) moves `limit_hit`
//!   steps on through `on_limit` (hooked in [`Executor::on_host_event`]),
//!   `retry_limit` (timers) and [`Effect::Handoff`]; [`Engine::switch_runtime`]
//!   is the manual switch.
//! - **Placement:** every agent session goes to [`EngineConfig::host`]; the
//!   runtime is picked from the workflow's `runtime.chain`
//!   ([`EngineConfig::adapter`] without one).

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde_json::{Value, json};
use sun_core::host_protocol::{
    DeviceFlowRequest, DeviceInput, DeviceResult, DeviceScreen, ExecEnvCapability, PreviewSide,
    Worktree,
};
use sun_core::model::{
    AuditAction, EventRecord, HostRuntime, Note, NoteKind, OutboxStatus, Run, RuntimeSource,
    SecretScope, Step, WorkflowOwner,
};
use sun_core::{
    BoxStream, EngineHandle, Error, ExecEnvSpec, HostEvent, HostId, HostLink, IdemKey, ModLink,
    NoteId, Result, RunId, RunStatus, SessionHandle, SessionId, SessionSpec, StateMachine,
    StepBrief, StepId, StepKey, StepStatus, StopMode, SunEvent, TaskId, WorkflowId,
};
use sun_store::{
    AgentRepo, ArtifactRepo, AuditRepo, ContextRepo, EventLog, HostRepo, NewEvent, NoteRepo,
    OutboxRepo, RepoRepo, RunRepo, RuntimeRepo, SecretRepo, SessionRepo, SettingRepo, Store,
    TaskRepo, Tx, WorkflowRepo, WorkspaceRepo,
};
use sun_workflow::{Step as StepDef, StepKind, Workflow};
use tokio::sync::{Notify, mpsc, oneshot};
use tokio::task::JoinHandle;

use crate::approval::{self, OnTimeout, Principal};
use crate::device;
use crate::gates::{self, GateResult, Verdict};
use crate::multi_repo;
use crate::outbox::{self, Outcome, Settlement};
use crate::push;
use crate::queue;
use crate::state::{self, RunOutcome};

#[path = "agent.rs"]
pub(crate) mod agent;
#[path = "context.rs"]
mod context;
#[path = "corrections.rs"]
pub(crate) mod corrections;
#[path = "decision.rs"]
mod decision;
#[path = "dispatch.rs"]
mod dispatch;
#[path = "failover.rs"]
pub(crate) mod failover;
#[path = "pause.rs"]
mod pause;
#[path = "repo_source.rs"]
pub(crate) mod repo_source;
#[path = "resolve.rs"]
mod resolve;
#[path = "retry.rs"]
pub(crate) mod retry;
#[path = "rotation.rs"]
mod rotation;
#[path = "tasks.rs"]
pub(crate) mod tasks;
#[path = "watchdog.rs"]
mod watchdog;
pub(crate) use context::read_design;
pub use context::{Design, ScreenPath, SessionMcp};
pub use decision::{Answer, Decision, DecisionClient, DecisionReplay, PLUGIN_METHOD, Tally};
pub(crate) use decision::{Hook, Settled, Triage, triage};
pub use resolve::Resolution;
pub use tasks::{Actor, NewTask, TaskChange, WorkflowPick};
pub use watchdog::{StallPolicy, WatchdogConfig};

/// Event kinds the executor writes (besides the agent events from hosts).
pub(crate) mod kind {
    pub const STEP_STATUS: &str = "step_status";
    pub const RUN_STATUS: &str = "run_status";
    /// A session id was chosen for a step attempt (before the host call).
    pub const SESSION_ASSIGNED: &str = "session_assigned";
    /// The host started the session; payload has the full handle.
    pub const SESSION_STARTED: &str = "session_started";
    pub const DISPATCH_FAILED: &str = "dispatch_failed";
    pub const HOST_UNREACHABLE: &str = "host_unreachable";
    pub const APPROVAL: &str = "approval";
    /// An approval `timeout: {then: remind}` fired; the step keeps waiting.
    pub const APPROVAL_REMINDER: &str = "approval_reminder";
    pub const OUTBOX_DELIVERED: &str = "outbox_delivered";
    pub const OUTBOX_FAILED: &str = "outbox_failed";
    /// Surfaced for a human; the step waits.
    pub const OUTBOX_UNKNOWN: &str = "outbox_unknown";
    /// An agent note (`sun.note`).
    pub const NOTE: &str = "note";
    /// A human steered a running agent; payload `{text, session_id}`.
    pub const INTERVENTION: &str = "intervention";
    /// The context pack a session was given; payload
    /// `{step_id, context_pack_id, summary}`.
    pub const CONTEXT_PACK: &str = "context_pack";
    /// An extension step's mod answer; payload `{step_id, output | error}`.
    pub const EXTENSION_RESULT: &str = "extension_result";
}

/// Where and how the executor runs things.
#[derive(Debug, Clone)]
pub struct EngineConfig {
    /// Host of runs started without a placement ([`Engine::start_run`]);
    /// the queue places each run itself ([`Engine::start_run_on`]).
    pub host: HostId,
    /// `RuntimeAdapter::id` of agent sessions of workflows without `runtime:`.
    pub adapter: String,
    /// A mod call without an answer after this is `unknown`, never retried.
    pub delivery_timeout: Duration,
    /// Stall, loop and budget rules.
    pub watchdog: WatchdogConfig,
    /// The operator allows `bypass_permissions` (sun.yaml
    /// `runtimes.allow_bypass`). Off, no session runs in it, whatever was
    /// saved while it was on ([`failover::bypass_disabled`]).
    pub allow_bypass: bool,
}

/// Handle to the running executor. Cheap to clone.
///
/// Stop the engine by aborting the task [`Engine::start`] returned; all state
/// is in the store and the next start resumes it.
#[derive(Clone)]
pub struct Engine {
    commands: mpsc::Sender<Command>,
    pub(crate) store: Store,
    pub(crate) hosts: Arc<dyn HostLink>,
    pub(crate) mods: Arc<dyn ModLink>,
    /// `EngineConfig::host`: where engine-side checks run (`quality`).
    pub(crate) host: HostId,
    /// `EngineConfig::allow_bypass`, for placement (`queue`).
    pub(crate) allow_bypass: bool,
    /// Wakes the knowledge worker after an ingest (`knowledge`).
    pub(crate) knowledge_wake: Arc<Notify>,
    /// Wakes the outbox worker after a row enqueued outside the executor (`memory`).
    pub(crate) outbox_wake: Arc<Notify>,
    /// The decision backend (`triggers` triage, `corrections` causes).
    pub(crate) decisions: decision::Backend,
    /// Git connection tokens (`vcs`).
    pub(crate) vcs: crate::vcs::Vcs,
}

impl std::fmt::Debug for Engine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Engine").finish_non_exhaustive()
    }
}

/// Where runs of a runtime may go ([`Engine::ready_hosts`]).
pub(crate) enum Ready {
    /// Online hosts with the runtime installed and logged in, oldest first
    /// (may be empty: the run waits), each with the exec env kinds it has
    /// ready (`bare` always) and, under `v2`, its cap on sessions of the
    /// runtime.
    Hosts(Vec<(HostId, Vec<String>, Option<usize>)>),
    /// No host has reported the runtime at all (not connected yet, tests).
    Unreported,
}

/// Setting: hours a succeeded or cancelled task's worktrees are kept before
/// they are removed; unset = 0. `sun serve` writes it from sun.yaml.
pub const WORKTREE_RETENTION: &str = "worktree_retention_hours";

/// Setting: where placement reads what each host's runtimes are, `"v1"`
/// (the host's raw capabilities report, the default) or `"v2"` (its
/// `host_runtime` rows, with per-host `max_sessions`). `sun serve` writes it
/// from sun.yaml `runtimes.model`, the kill switch of the runtime rework.
pub const RUNTIMES_MODEL: &str = "runtimes_model";

/// [`RUNTIMES_MODEL`] is `v2`: every host's `host_runtime` rows; `None`
/// under `v1`.
pub(crate) async fn runtime_rows(tx: &mut Tx) -> Result<Option<Vec<HostRuntime>>> {
    Ok(match tx.setting(RUNTIMES_MODEL).await? {
        Some(model) if model == "v2" => Some(tx.host_runtimes(None).await?),
        _ => None,
    })
}

/// Setting: worktree removals still to do ([`Removal`]), so a restart
/// schedules them again instead of leaking the worktrees.
const PENDING_REMOVALS: &str = "worktree_removals";

/// A finished task's worktrees, due for removal at `due_at`.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
struct Removal {
    host: HostId,
    task_id: TaskId,
    due_at: chrono::DateTime<chrono::Utc>,
}

async fn pending_removals(tx: &mut Tx) -> Result<Vec<Removal>> {
    let pending = tx.setting(PENDING_REMOVALS).await?;
    Ok(pending
        .and_then(|v| serde_json::from_value(v).ok())
        .unwrap_or_default())
}

/// Records, in the transaction that ends the run, that the task's worktrees
/// go once [`WORKTREE_RETENTION`] has passed.
async fn plan_removal(tx: &mut Tx, host: HostId, task_id: TaskId) -> Result<Removal> {
    let keep = tx.setting(WORKTREE_RETENTION).await?;
    // Clamped: a century is "keep".
    let hours = keep.and_then(|h| h.as_i64()).unwrap_or(0).clamp(0, 876_000);
    let removal = Removal {
        host,
        task_id,
        due_at: chrono::Utc::now() + chrono::Duration::hours(hours),
    };
    let mut pending = pending_removals(tx).await?;
    pending.retain(|r| r.task_id != task_id);
    pending.push(removal.clone());
    tx.put_setting(PENDING_REMOVALS, &json!(pending)).await?;
    Ok(removal)
}

/// Removes the task's worktrees, then its pending entry; a failure keeps
/// the entry for the next start.
async fn remove_worktrees(hosts: &dyn HostLink, store: &Store, removal: &Removal) -> Result<()> {
    hosts.remove_worktree(removal.host, removal.task_id).await?;
    let mut tx = store.begin().await?;
    let mut pending = pending_removals(&mut tx).await?;
    pending.retain(|r| r.task_id != removal.task_id);
    tx.put_setting(PENDING_REMOVALS, &json!(pending)).await?;
    Ok(tx.commit().await?)
}

/// Inputs to the engine task from outside callers and timers.
enum Command {
    StartRun {
        run_id: RunId,
        /// Where the queue placed it; `None`: `EngineConfig::host`.
        placed: Option<queue::Placement>,
        /// Read the workflow from the task's repo first (`repo_source`);
        /// off for sandbox runs of a given workflow (`eval`, `dry_run`).
        from_repo: bool,
        reply: oneshot::Sender<Result<()>>,
    },
    Decide {
        run_id: RunId,
        step_key: StepKey,
        approved: bool,
        by: Principal,
        /// Why a human rejected (`Engine::reject`).
        reason: Option<String>,
        reply: oneshot::Sender<Result<()>>,
    },
    /// The agent claimed done through `sun.done`.
    Done {
        run_id: RunId,
        step_key: StepKey,
        summary: String,
        reply: oneshot::Sender<Result<()>>,
    },
    /// The queue may not place a queued run: it fails, journalled as
    /// `kind` with `payload` (whose `reason` ends the run).
    Refuse {
        run_id: RunId,
        kind: &'static str,
        payload: Value,
        reply: oneshot::Sender<Result<()>>,
    },
    /// A human cancelled the run.
    Cancel {
        run_id: RunId,
        reply: oneshot::Sender<Result<()>>,
    },
    /// A human steered the run's running agent session.
    Steer {
        run_id: RunId,
        text: String,
        reply: oneshot::Sender<Result<EventRecord>>,
    },
    /// A `wait` step's timer or an approval timeout fired.
    Wake(StepId),
    /// Something a running gate waits for happened (a PR merged): check again.
    Recheck(RunId),
    /// A watchdog deadline of a running agent step fired.
    Watch(StepId),
    /// Try an agent step whose host was unreachable again (`dispatch`).
    Retry(StepId),
    /// A gate step's checks finished.
    Gated {
        step_id: StepId,
        results: Vec<GateResult>,
        /// "Is the diff in scope?", asked when the gates passed.
        scope: Option<Settled>,
    },
    /// A decision hook of a step answered (`watchdog`, `failover`).
    Hooked {
        step_id: StepId,
        decides: &'static str,
        settled: Settled,
    },
    /// The push before an `open_pr` finished (see `push`); a multi-repo
    /// task's `Ok` names the repos that changed.
    Pushed {
        step_id: StepId,
        result: std::result::Result<Option<Vec<String>>, String>,
    },
    /// A task-folder snapshot finished (see `snapshot`).
    Snapshotted {
        step_id: StepId,
        result: std::result::Result<Vec<(String, Vec<u8>)>, String>,
    },
    /// A device verify step's run finished (see `device`).
    Device {
        step_id: StepId,
        result: Result<DeviceResult>,
    },
    /// An extension step's mod call answered.
    Extended {
        step_id: StepId,
        result: Result<Value>,
    },
    /// A decide step's question was answered (see `decision`).
    Decided {
        step_id: StepId,
        result: Result<Decision>,
    },
    /// A human switches a step's runtime.
    Switch {
        run_id: RunId,
        step_key: StepKey,
        to: String,
        reply: oneshot::Sender<Result<()>>,
    },
    /// An agent asks a human (`sun.ask`); the reply yields the answer later.
    Ask {
        run_id: RunId,
        step_key: StepKey,
        question: String,
        options: Vec<String>,
        reply: oneshot::Sender<Result<oneshot::Receiver<String>>>,
    },
    /// A human answers an agent's question.
    Answer {
        run_id: RunId,
        question_id: NoteId,
        answer: String,
        reply: oneshot::Sender<Result<()>>,
    },
    /// A person moves a task (`tasks`).
    MoveTask {
        task_id: TaskId,
        to: sun_core::TaskStatus,
        actor: Option<tasks::Actor>,
        reply: oneshot::Sender<Result<()>>,
    },
    /// A person assigns a task (`tasks`).
    AssignTask {
        task_id: TaskId,
        to: Option<sun_core::model::Assignee>,
        actor: Option<tasks::Actor>,
        reply: oneshot::Sender<Result<()>>,
    },
    /// A person commented on a task assigned to an agent (`tasks`).
    Comment {
        task_id: TaskId,
        text: String,
        actor: Option<tasks::Actor>,
        reply: oneshot::Sender<Result<()>>,
    },
    /// A human pauses or resumes a run (`pause`).
    Pause {
        run_id: RunId,
        reply: pause::Reply,
    },
    Resume {
        run_id: RunId,
        reply: pause::Reply,
    },
    /// A human retries a step (`retry`).
    RetryStep {
        run_id: RunId,
        step_key: StepKey,
        by: String,
        reply: retry::Reply,
    },
    /// A human decides an `unknown` outbox row (`resolve`).
    ResolveOutbox {
        id: sun_core::OutboxId,
        resolution: Resolution,
        by: String,
        reply: retry::Reply,
    },
    /// An admin kills a live session (`pause`).
    Kill {
        session_id: SessionId,
        reason: String,
        by: String,
        reply: oneshot::Sender<Result<Option<RunId>>>,
    },
    /// A human answers an escalated decide step (`corrections`).
    CorrectDecision {
        run_id: RunId,
        step_key: StepKey,
        by: Principal,
        answer: Value,
        reply: corrections::Reply,
    },
}

/// Work done after a commit; always re-derivable from the store.
enum Effect {
    Start(Start),
    Stop(SessionHandle),
    Timer {
        step_id: StepId,
        after: Duration,
    },
    WakeOutbox,
    /// Read a repo-less task's folder off the engine task (`snapshot`).
    Snapshot {
        step_id: StepId,
        job: crate::snapshot::Job,
    },
    /// Run a gate step's checks off the engine task, then the scope
    /// question when they pass.
    Gate {
        step_id: StepId,
        job: gates::Job,
        scope: Option<(Hook, DecisionClient)>,
    },
    /// Ask a decision hook off the engine task ([`Command::Hooked`]).
    Ask {
        step_id: StepId,
        hook: Hook,
        client: DecisionClient,
    },
    /// Call an extension step's mod off the engine task.
    Extension {
        step_id: StepId,
        call: sun_core::ModCall,
    },
    /// Push the task branch off the engine task (`push`).
    Push {
        step_id: StepId,
        job: push::Job,
    },
    /// Run a device verify off the engine task.
    Device {
        step_id: StepId,
        job: device::Job,
    },
    /// Checkpoint, build the hand-off pack and start the next session (`failover`).
    Handoff(failover::Handoff),
    /// Ask a decide step's question off the engine task.
    Decide {
        step_id: StepId,
        job: decision::Job,
    },
    /// Remove a finished task's worktrees from its host; first, for
    /// a run that ended `done (unverified)`, suggest a gate from its repo
    /// (`gates::suggest`).
    RemoveWorktree {
        removal: Removal,
        suggest: Option<(RunId, Worktree)>,
    },
}

/// Starting one agent session on the host ([`Effect::Start`]).
#[derive(Clone)]
struct Start {
    step: Step,
    session_id: SessionId,
    adapter: String,
    prompt: String,
    /// Set when the session continues another's work (P1 of its pack).
    handoff: Option<Box<sun_context::HandoffPack>>,
    /// Continue the CLI's own earlier session first (`dispatch`).
    resume: Option<dispatch::Resume>,
    /// Where the session runs: the task's override, else the workflow's.
    exec_env: ExecEnvSpec,
    /// The runtime row's `byo_key` source (`None` for other sources).
    byo_key: Option<sun_core::ByoKey>,
    /// The chain entry's model, if it sets one.
    model: Option<String>,
    /// The chain entry's effort, if it sets one the host offers.
    effort: Option<String>,
    /// The step's own mode (an agent's, on its system workflow); `None`:
    /// the host's `sun.yaml` default.
    permission_mode: Option<String>,
}

impl Engine {
    /// Resumes every running run ("resume on boot"), then spawns the
    /// engine task and the outbox worker. The task ends only on a store
    /// failure (fail-stop: restart it and it resumes) or when aborted.
    pub async fn start(
        store: Store,
        hosts: Arc<dyn HostLink>,
        mods: Arc<dyn ModLink>,
        config: EngineConfig,
    ) -> Result<(Self, JoinHandle<Result<()>>)> {
        Self::start_with(store, hosts, mods, config, None, None).await
    }

    /// [`Engine::start`] with a decision backend for `decide` steps and
    /// the `sun` MCP server for every agent session (`mcp`). Without a
    /// backend, every `decide` step escalates to a human.
    pub async fn start_with(
        store: Store,
        hosts: Arc<dyn HostLink>,
        mods: Arc<dyn ModLink>,
        config: EngineConfig,
        decisions: Option<DecisionClient>,
        mcp: Option<SessionMcp>,
    ) -> Result<(Self, JoinHandle<Result<()>>)> {
        let (commands, inbox) = mpsc::channel(256);
        let link = hosts.clone();
        // Subscribe before resuming so no host event is missed.
        let host_events = hosts.events();
        let wake = Arc::new(Notify::new());
        let delivery_timeout = config.delivery_timeout;
        let default_host = config.host;
        let allow_bypass = config.allow_bypass;
        let decisions =
            decision::Backend::new(decision::switched(&mut store.read().await?, decisions).await?);
        let vcs = crate::vcs::Vcs::new(store.clone());
        let mut executor = Executor {
            vcs: vcs.clone(),
            store: store.clone(),
            hosts,
            watchdog: watchdog::Watchdog::new(config.watchdog.clone()),
            mods: mods.clone(),
            config,
            commands: commands.clone(),
            wake: wake.clone(),
            workflows: HashMap::new(),
            sessions: HashMap::new(),
            mcp,
            mcp_tokens: HashMap::new(),
            outstanding: HashMap::new(),
            in_flight: Default::default(),
            gating: HashSet::new(),
            rechecks: HashSet::new(),

            decisions: decisions.clone(),
            failover: failover::Failover::load(&mut store.read().await?).await?,
            questions: HashMap::new(),
            dispatch: Default::default(),
            placed: HashMap::new(),
            based: HashSet::new(),
            rotations: HashMap::new(),
            rotation_guard: Default::default(),
            retry_timers: HashSet::new(),
        };
        executor.resume().await?;
        // The worker starts after resume rebuilt `outstanding`, so every
        // settlement finds its step.
        let (settle, settled) = mpsc::channel(64);
        let mods_handle = mods.clone();
        let worker = AbortOnDrop(tokio::spawn(outbox::run(
            store.clone(),
            mods,
            vcs.clone(),
            delivery_timeout,
            wake.clone(),
            settle,
            executor.in_flight.clone(),
        )));
        let task = tokio::spawn(executor.run(inbox, settled, host_events, worker));
        Ok((
            Self {
                commands,
                store,
                hosts: link,
                mods: mods_handle,
                host: default_host,
                allow_bypass,
                knowledge_wake: Arc::default(),
                outbox_wake: wake,
                decisions,
                vcs,
            },
            task,
        ))
    }

    /// Starts a `queued` run: `queued → running`, then its first steps.
    pub async fn start_run(&self, run_id: RunId) -> Result<()> {
        self.request(|reply| Command::StartRun {
            run_id,
            placed: None,
            from_repo: true,
            reply,
        })
        .await
    }

    /// [`Self::start_run`] on the pinned workflow version, never the repo's
    /// file: sandbox runs of a given workflow (`eval`, `dry_run`).
    pub(crate) async fn start_pinned(&self, run_id: RunId) -> Result<()> {
        self.request(|reply| Command::StartRun {
            run_id,
            placed: None,
            from_repo: false,
            reply,
        })
        .await
    }

    /// [`Self::start_run`] placed on `host`: its worktree, sessions, gates
    /// and push all run there (journaled as `run_placed`).
    pub async fn start_run_on(&self, run_id: RunId, host: HostId) -> Result<()> {
        self.start_placed(run_id, queue::Placement::on(host)).await
    }

    /// [`Self::start_run_on`] with what the queue counted it as.
    pub(crate) async fn start_placed(&self, run_id: RunId, placed: queue::Placement) -> Result<()> {
        self.request(|reply| Command::StartRun {
            run_id,
            placed: Some(placed),
            from_repo: true,
            reply,
        })
        .await
    }

    /// Fails queued run `run_id` the queue may not place, journalling why
    /// as `kind` with `payload` (its `reason` is the run's end reason). It
    /// never starts a step.
    pub(crate) async fn refuse_run(
        &self,
        run_id: RunId,
        kind: &'static str,
        payload: Value,
    ) -> Result<()> {
        self.request(|reply| Command::Refuse {
            run_id,
            kind,
            payload,
            reply,
        })
        .await
    }

    /// Hosts a run of runtime `adapter` may be placed on: online, with the
    /// adapter installed and logged in, read per [`RUNTIMES_MODEL`]. Under
    /// `v2` each carries its session cap: the host's reported
    /// `max_sessions`, else `max_sessions` (sun.yaml's default).
    pub(crate) async fn ready_hosts(
        &self,
        adapter: &str,
        max_sessions: Option<usize>,
    ) -> Result<Ready> {
        let mut read = self.store.read().await?;
        let rows = runtime_rows(&mut read).await?;
        let mut reported = false;
        let mut ready = Vec::new();
        for host in read.hosts().await? {
            let Some(report) = failover::Reported::of(&host, rows.as_deref()) else {
                continue;
            };
            reported |= report.has(adapter);
            if self.hosts.is_online(host.id) && report.unready(adapter).is_none() {
                let envs = host.capabilities.get("exec_envs").cloned();
                let envs: Vec<ExecEnvCapability> = envs
                    .and_then(|e| serde_json::from_value(e).ok())
                    .unwrap_or_default();
                let envs = envs.into_iter().filter(|e| e.ready && e.kind != "bare");
                let kinds = std::iter::once("bare".to_owned()).chain(envs.map(|e| e.kind));
                let cap = match &report {
                    failover::Reported::Rows(_) => report.max_sessions(adapter).or(max_sessions),
                    failover::Reported::Caps(_) => None,
                };
                ready.push((host.id, kinds.collect(), cap));
            }
        }
        Ok(match reported {
            true => Ready::Hosts(ready),
            false => Ready::Unreported,
        })
    }

    /// Records an approval decision on a `waiting_approval` step.
    /// `by` must match the step's `who:` (`user:<name>` or `role:<role>`),
    /// else `Forbidden` and nothing changes. The only way an approval
    /// moves, besides its own `timeout:` policy.
    pub async fn decide(
        &self,
        run_id: RunId,
        step_key: StepKey,
        approved: bool,
        by: Principal,
    ) -> Result<()> {
        self.request(|reply| Command::Decide {
            run_id,
            step_key,
            approved,
            by,
            reason: None,
            reply,
        })
        .await
    }

    /// Moves a `running` or `limit_hit` agent step to runtime `to` (one of
    /// the run's chain) with a hand-off pack, in the same worktree.
    /// A run paused by `on_limit: pause_and_notify` resumes.
    pub async fn switch_runtime(&self, run_id: RunId, step_key: StepKey, to: String) -> Result<()> {
        self.request(|reply| Command::Switch {
            run_id,
            step_key,
            to,
            reply,
        })
        .await
    }

    /// Answers question `question_id` an agent of `run_id` asked with
    /// `sun.ask` (`POST /runs/{id}/questions/{qid}/answer`). When the agent
    /// gave options, `answer` must be one of them (`InvalidInput`). The
    /// agent's step runs again.
    pub async fn answer(&self, run_id: RunId, question_id: NoteId, answer: String) -> Result<()> {
        self.request(|reply| Command::Answer {
            run_id,
            question_id,
            answer,
            reply,
        })
        .await
    }

    /// A PR of the run's task merged: a waiting `merge_order` gate checks
    /// again (`multi_repo`). Fire and forget.
    pub(crate) async fn recheck(&self, run_id: RunId) {
        let _ = self.commands.send(Command::Recheck(run_id)).await;
    }

    /// Fires `step_id`'s timer now (`on_wake`).
    pub(crate) async fn wake(&self, step_id: StepId) {
        let _ = self.commands.send(Command::Wake(step_id)).await;
    }

    /// Cancels a queued, running or paused run: open steps are
    /// cancelled, their sessions stopped, the task follows. A finished run
    /// is `InvalidTransition`.
    pub async fn cancel(&self, run_id: RunId) -> Result<()> {
        self.request(|reply| Command::Cancel { run_id, reply })
            .await
    }

    /// Sends a message to the run's running agent session and
    /// journals it as an `intervention` event. `InvalidInput` when no agent
    /// session of the run is live.
    pub async fn steer(&self, run_id: RunId, text: String) -> Result<EventRecord> {
        self.request(|reply| Command::Steer {
            run_id,
            text,
            reply,
        })
        .await
    }

    async fn request<T>(
        &self,
        command: impl FnOnce(oneshot::Sender<Result<T>>) -> Command,
    ) -> Result<T> {
        let stopped = || Error::Unavailable("engine stopped".into());
        let (reply, answer) = oneshot::channel();
        self.commands
            .send(command(reply))
            .await
            .map_err(|_| stopped())?;
        answer.await.map_err(|_| stopped())?
    }
}

/// Aborts the outbox worker when the engine task ends or is aborted, so two
/// workers never deliver from the same store.
struct AbortOnDrop(JoinHandle<Result<()>>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

struct Executor {
    store: Store,
    /// Git connection tokens for pushes (`vcs`).
    vcs: crate::vcs::Vcs,
    hosts: Arc<dyn HostLink>,
    mods: Arc<dyn ModLink>,
    config: EngineConfig,
    commands: mpsc::Sender<Command>,
    wake: Arc<Notify>,
    /// Parsed pinned workflow versions.
    workflows: HashMap<(WorkflowId, u32), Arc<Workflow>>,
    /// Live sessions → the step attempt they work for. Routes host events.
    sessions: HashMap<SessionId, (StepId, SessionHandle)>,
    /// The `sun` MCP server for sessions, if wired.
    mcp: Option<SessionMcp>,
    /// Each live session's MCP token, revoked with [`Self::forget_session`].
    // ponytail: in memory; sessions re-attached after a kernel restart lose
    // `sun` MCP until their next session. Persist tokens if that matters.
    mcp_tokens: HashMap<SessionId, String>,
    /// Undelivered outbox rows of running action/notify steps.
    outstanding: HashMap<IdemKey, StepId>,
    /// Outbox rows the worker is delivering now (no human decision yet).
    in_flight: outbox::InFlight,
    watchdog: watchdog::Watchdog,
    /// Gate and device steps whose checks are running now.
    gating: HashSet<StepId>,
    /// Runs with a merge since their `merge_order` check last read the PRs:
    /// a check still in flight then checks again when it lands.
    rechecks: HashSet<RunId>,
    failover: failover::Failover,
    /// The `/v1/systemone` backend of `decide` steps and hooks, if installed.
    decisions: decision::Backend,
    /// Open `sun.ask` questions → their step and the waiting agent call.
    questions: HashMap<NoteId, (StepId, oneshot::Sender<String>)>,
    /// Resumes in flight and retries of unreachable hosts.
    dispatch: dispatch::Dispatch,
    /// The host of each placed active run (`run_placed`), filled on start and
    /// on resume. Runs without one use `config.host`.
    placed: HashMap<RunId, HostId>,
    /// Runs whose base commits were journaled (`run_base`) since start.
    // ponytail: never pruned, like `placed`; a restart journals them again
    // (same commits; readers take the first).
    based: HashSet<RunId>,
    /// Sessions past the context threshold → `(context_used, window)`;
    /// rotated at their next tool result (`rotation`).
    rotations: HashMap<SessionId, (u32, u32)>,
    /// Stops a fresh session that starts full from rotating again (`rotation`).
    rotation_guard: rotation::Guard,
    /// Failed attempts whose `retry:` policy timer is armed (`retry`).
    retry_timers: HashSet<StepId>,
}

impl Executor {
    /// The host run `run_id` lives on (its worktree is there).
    fn host(&self, run_id: RunId) -> HostId {
        self.placed
            .get(&run_id)
            .copied()
            .unwrap_or(self.config.host)
    }

    async fn run(
        mut self,
        mut inbox: mpsc::Receiver<Command>,
        mut settled: mpsc::Receiver<Settlement>,
        mut host_events: BoxStream<'static, HostEvent>,
        mut worker: AbortOnDrop,
    ) -> Result<()> {
        let mut hosts_open = true;
        loop {
            tokio::select! {
                Some(command) = inbox.recv() => self.handle(command).await?,
                Some(settlement) = settled.recv() => self.on_settled(settlement).await?,
                event = next(&mut host_events), if hosts_open => match event {
                    Some(event) => self.on_host_event(event).await?,
                    None => hosts_open = false,
                },
                ended = &mut worker.0 => {
                    return Err(match ended {
                        Ok(Err(e)) => e,
                        _ => Error::Unavailable("outbox worker stopped".into()),
                    });
                }
            }
        }
    }

    async fn handle(&mut self, command: Command) -> Result<()> {
        match command {
            Command::StartRun {
                run_id,
                placed,
                from_repo,
                reply,
            } => {
                let _ = reply.send(self.start_run(run_id, placed, from_repo).await);
            }
            Command::Decide {
                run_id,
                step_key,
                approved,
                by,
                reason,
                reply,
            } => {
                let _ = reply.send(self.decide(run_id, step_key, approved, by, reason).await);
            }
            Command::Pause { run_id, reply } => {
                let _ = reply.send(self.pause_run(run_id).await);
            }
            Command::MoveTask {
                task_id,
                to,
                actor,
                reply,
            } => {
                let _ = reply.send(self.move_task(task_id, to, actor, None).await);
            }
            Command::AssignTask {
                task_id,
                to,
                actor,
                reply,
            } => {
                let _ = reply.send(self.assign_task(task_id, to, actor).await);
            }
            Command::Comment {
                task_id,
                text,
                actor,
                reply,
            } => {
                let _ = reply.send(self.on_comment(task_id, text, actor).await);
            }
            Command::Resume { run_id, reply } => {
                let _ = reply.send(self.resume_run(run_id).await);
            }
            Command::Kill {
                session_id,
                reason,
                by,
                reply,
            } => {
                let _ = reply.send(self.kill_session(session_id, &reason, &by).await);
            }
            Command::Done {
                run_id,
                step_key,
                summary,
                reply,
            } => {
                let _ = reply.send(self.done(run_id, step_key, summary).await);
            }
            Command::Refuse {
                run_id,
                kind,
                payload,
                reply,
            } => {
                let _ = reply.send(self.refuse_run(run_id, kind, payload).await);
            }
            Command::Cancel { run_id, reply } => {
                let _ = reply.send(self.cancel(run_id).await);
            }
            Command::Steer {
                run_id,
                text,
                reply,
            } => {
                let _ = reply.send(self.steer(run_id, text).await);
            }
            Command::Wake(step_id) => self.on_wake(step_id).await?,
            Command::Recheck(run_id) => self.recheck(run_id).await?,
            Command::Watch(step_id) => self.on_watch(step_id).await?,
            Command::Retry(step_id) => self.on_retry(step_id).await?,
            Command::Gated {
                step_id,
                results,
                scope,
            } => self.on_gated(step_id, results, scope).await?,
            Command::Hooked {
                step_id,
                decides,
                settled,
            } => self.on_hooked(step_id, decides, settled).await?,
            Command::Device { step_id, result } => self.on_device(step_id, result).await?,
            Command::Pushed { step_id, result } => self.on_pushed(step_id, result).await?,
            Command::Snapshotted { step_id, result } => self.on_snapshot(step_id, result).await?,
            Command::Decided { step_id, result } => self.on_decided(step_id, result).await?,
            Command::Extended { step_id, result } => self.on_extended(step_id, result).await?,
            Command::Switch {
                run_id,
                step_key,
                to,
                reply,
            } => {
                let _ = reply.send(self.switch(run_id, step_key, to).await);
            }
            Command::Ask {
                run_id,
                step_key,
                question,
                options,
                reply,
            } => {
                let _ = reply.send(self.ask(run_id, step_key, question, options).await);
            }
            Command::Answer {
                run_id,
                question_id,
                answer,
                reply,
            } => {
                let _ = reply.send(self.answer(run_id, question_id, answer).await);
            }
            Command::RetryStep {
                run_id,
                step_key,
                by,
                reply,
            } => {
                let _ = reply.send(self.retry_step(run_id, step_key, by).await);
            }
            Command::ResolveOutbox {
                id,
                resolution,
                by,
                reply,
            } => {
                let _ = reply.send(self.resolve_outbox(id, resolution, by).await);
            }
            Command::CorrectDecision {
                run_id,
                step_key,
                by,
                answer,
                reply,
            } => {
                let _ = reply.send(self.correct_decision(run_id, step_key, by, answer).await);
            }
        }
        Ok(())
    }

    async fn start_run(
        &mut self,
        run_id: RunId,
        placed: Option<queue::Placement>,
        from_repo: bool,
    ) -> Result<()> {
        let host = placed.as_ref().map(|p| p.host);
        // The repo's workflow and exec env are read before the transaction
        // (host I/O), and only on a host the lease allows.
        let (source, repo_env) = match from_repo {
            true => {
                let mut read = self.store.read().await?;
                let run = read.get_run(run_id).await?;
                leased(&mut read, run.task_id, host).await?;
                let name = read.get_workflow(run.workflow_id).await?.name;
                let worktree = worktree_of(&mut read, run.task_id).await?;
                drop(read);
                let on = host.unwrap_or(self.config.host);
                let hosts = self.hosts.as_ref();
                (
                    repo_source::read(hosts, on, &worktree, &name).await,
                    repo_source::read_exec_env(hosts, on, &worktree).await,
                )
            }
            false => (Ok(None), Ok(None)),
        };
        let mut tx = self.store.begin().await?;
        // Lease before start: a held or leased task runs only on its seller's
        // host, checked here in the start's own transaction (no race with a
        // hold committed after the queue read).
        let run = tx.get_run(run_id).await?;
        leased(&mut tx, run.task_id, host).await?;
        if let Some(placed) = &placed {
            let payload =
                serde_json::to_value(placed).map_err(|e| Error::Protocol(e.to_string()))?;
            tx.append_event(NewEvent::new(run_id, None, queue::RUN_PLACED, payload))
                .await?;
        }
        move_run(&mut tx, run_id, RunStatus::Running, None).await?;
        if let Some(host) = host {
            self.placed.insert(run_id, host);
        }
        let mut effects = Vec::new();
        let started = match (source, repo_env) {
            (Ok(source), Ok(repo_env)) => {
                if from_repo {
                    // `null` when the repo declares none (any more).
                    let key = repo_exec_env_key(run.task_id);
                    tx.put_setting(&key, &json!(repo_env)).await?;
                }
                match source {
                    Some(source) => repo_source::pin(&mut tx, &run, source).await?,
                    None => Ok(()),
                }
            }
            (Err(why), _) | (_, Err(why)) => Err(why),
        };
        if let Err(why) = started {
            let (mut none, failed) = (HashMap::new(), RunStatus::Failed);
            self.end_run(
                &mut tx,
                run_id,
                &mut none,
                failed,
                &why,
                false,
                &mut effects,
            )
            .await?;
        }
        self.advance(&mut tx, run_id, &mut effects).await?;
        tx.commit().await?;
        self.apply(effects).await
    }

    /// [`Engine::refuse_run`]: `queued → running → failed` (a queued run has
    /// no edge to `failed`), with no step started.
    async fn refuse_run(&mut self, run_id: RunId, kind: &str, payload: Value) -> Result<()> {
        let reason = payload["reason"].as_str().unwrap_or(kind).to_owned();
        let mut tx = self.store.begin().await?;
        tx.append_event(NewEvent::new(run_id, None, kind, payload))
            .await?;
        move_run(&mut tx, run_id, RunStatus::Running, None).await?;
        let mut effects = Vec::new();
        let failed = RunStatus::Failed;
        self.end_run(
            &mut tx,
            run_id,
            &mut HashMap::new(),
            failed,
            &reason,
            false,
            &mut effects,
        )
        .await?;
        tx.commit().await?;
        self.apply(effects).await
    }

    /// Ends a queued, running or paused run as `cancelled`: open steps are
    /// cancelled and their sessions stopped. A finished run is
    /// `InvalidTransition`.
    async fn cancel(&mut self, run_id: RunId) -> Result<()> {
        let mut tx = self.store.begin().await?;
        let mut latest = state::latest(tx.steps_of_run(run_id).await?);
        let mut effects = Vec::new();
        self.end_run(
            &mut tx,
            run_id,
            &mut latest,
            RunStatus::Cancelled,
            "cancelled by a human",
            false,
            &mut effects,
        )
        .await?;
        tx.commit().await?;
        self.apply(effects).await
    }

    /// Sends `text` to the run's live agent session and journals it as an
    /// `intervention`. `InvalidInput` when no agent is running.
    // ponytail: the engine task waits for the host's reply, like start_session.
    async fn steer(&mut self, run_id: RunId, text: String) -> Result<EventRecord> {
        let mut tx = self.store.read().await?;
        let running: HashSet<StepId> = tx
            .steps_of_run(run_id)
            .await?
            .into_iter()
            .filter(|s| s.status == StepStatus::Running)
            .map(|s| s.id)
            .collect();
        drop(tx);
        let (step_id, handle) = self
            .sessions
            .values()
            .find(|(step_id, _)| running.contains(step_id))
            .cloned()
            .ok_or_else(|| Error::InvalidInput("no agent is running in this run".into()))?;
        self.hosts
            .steer(&handle, sun_core::Steer { text: text.clone() })
            .await?;
        let mut tx = self.store.begin().await?;
        let step = tx.get_step(step_id).await?;
        let event = tx
            .append_event(NewEvent::new(
                run_id,
                Some(step.step_key.clone()),
                kind::INTERVENTION,
                json!({ "text": text, "session_id": handle.session_id }),
            ))
            .await?;
        corrections::on_steer(&mut tx, &step, &text).await?;
        tx.commit().await?;
        Ok(event)
    }

    async fn decide(
        &mut self,
        run_id: RunId,
        step_key: StepKey,
        approved: bool,
        by: Principal,
        reason: Option<String>,
    ) -> Result<()> {
        let mut tx = self.store.begin().await?;
        let run = tx.get_run(run_id).await?;
        let workflow = self.workflow(&mut tx, &run).await?;
        let allowed_as = approval::authorize(&workflow, &step_key, &by)?;
        let mut step = state::latest(tx.steps_of_run(run_id).await?)
            .remove(&step_key)
            .ok_or_else(|| Error::NotFound(format!("step `{step_key}` of run {run_id}")))?;
        // An agent step is decided only in its failover review, never while
        // it waits for an answer to its own `sun.ask`.
        let agent = workflow
            .steps
            .iter()
            .any(|d| d.id == step_key && matches!(d.kind, StepKind::Agent { .. }));
        if agent && !self.in_failover_review(&mut tx, &step).await? {
            return Err(Error::InvalidInput(format!(
                "step `{step_key}` is not waiting for a failover review"
            )));
        }
        let next = if approved {
            StepStatus::Succeeded
        } else {
            StepStatus::Rejected
        };
        move_step(&mut tx, &mut step, next, reason.as_deref()).await?;
        append(
            &mut tx,
            &step,
            kind::APPROVAL,
            json!({ "approved": approved, "by": by.user, "as": allowed_as, "reason": reason }),
        )
        .await?;
        tx.append_audit(
            &format!("user:{}", by.user),
            AuditAction::Approval,
            &format!("{run_id}/{step_key}"),
            if approved { "approved" } else { "rejected" },
        )
        .await?;
        let mut effects = Vec::new();
        if approved {
            self.accept_suggestion(&mut tx, &step, &by, &mut effects)
                .await?;
        } else {
            corrections::on_rejected(&mut tx, &step, reason.as_deref()).await?;
        }
        self.advance(&mut tx, run_id, &mut effects).await?;
        tx.commit().await?;
        self.apply(effects).await
    }

    /// Stores a host event exactly once, moves its step, then acks.
    async fn on_host_event(&mut self, event: HostEvent) -> Result<()> {
        // Unknown session: stale or not ours. Not acked; the host keeps it.
        let Some((step_id, _)) = self.sessions.get(&event.session_id).cloned() else {
            return Ok(());
        };
        let host_seq = i64::try_from(event.seq)
            .map_err(|_| Error::Protocol(format!("host seq {} out of range", event.seq)))?;
        let mut tx = self.store.begin().await?;
        let mut step = tx.get_step(step_id).await?;
        let record = NewEvent::agent(step.run_id, step.step_key.clone(), &event.event)?;
        let mut effects = Vec::new();
        let fresh = tx
            .append_host_event(event.session_id, host_seq, record)
            .await?
            .is_some();
        if fresh
            && self
                .resume_failed(
                    &mut tx,
                    &mut step,
                    event.session_id,
                    &event.event,
                    &mut effects,
                )
                .await?
        {
            // A fresh session takes over (`dispatch`).
        } else if fresh
            && !self
                .police(&mut tx, &mut step, &event.event, &mut effects)
                .await?
            && let Some(next) = state::after_agent_event(step.status, &event.event)
        {
            move_step(&mut tx, &mut step, next, None).await?;
            if next == StepStatus::Verifying {
                if let SunEvent::DoneClaimed { summary } = &event.event {
                    crate::conversation::reply(&mut tx, step.run_id, summary).await?;
                }
                // Agent steps have no inline gates; `gate` steps verify (see `gates`).
                self.finish_agent(&mut tx, &mut step, &mut effects).await?;
            }
            if let SunEvent::LimitHit { reset_at, .. } = &event.event {
                self.on_limit(&mut tx, &mut step, *reset_at, &mut effects)
                    .await?;
            }
            self.advance(&mut tx, step.run_id, &mut effects).await?;
        }
        // Still running and still this step's session (no hand-off above).
        if fresh
            && step.status == StepStatus::Running
            && self.sessions.contains_key(&event.session_id)
        {
            self.rotate_if_due(
                &mut tx,
                &mut step,
                event.session_id,
                &event.event,
                &mut effects,
            )
            .await?;
            if step.status == StepStatus::Running {
                self.switch_back_if_due(&mut tx, &mut step, &event.event, &mut effects)
                    .await?;
            }
        }
        tx.commit().await?;
        if matches!(event.event, SunEvent::Exited { .. }) {
            self.forget_session(event.session_id);
        }
        // An ack that doesn't arrive only means the host redelivers; the
        // `(session_id, seq)` constraint drops the duplicate.
        let _ = self.hosts.ack(event.session_id, event.seq).await;
        self.apply(effects).await
    }

    /// Records a delivery outcome and moves the step that enqueued it.
    async fn on_settled(&mut self, settlement: Settlement) -> Result<()> {
        let Settlement { item, outcome } = settlement;
        outbox::in_flight(&self.in_flight).remove(&item.id);
        let (status, kind, detail) = match outcome {
            Outcome::Delivered(reply) => (OutboxStatus::Delivered, kind::OUTBOX_DELIVERED, reply),
            Outcome::Failed(why) => (OutboxStatus::Failed, kind::OUTBOX_FAILED, json!(why)),
            Outcome::Unknown(why) => (OutboxStatus::Unknown, kind::OUTBOX_UNKNOWN, json!(why)),
        };
        let mut tx = self.store.begin().await?;
        if status != OutboxStatus::Unknown {
            let result = (status == OutboxStatus::Delivered).then_some(&detail);
            tx.set_outbox_status(item.id, status, result).await?;
        }
        let step_id = self.outstanding.get(&item.idem_key).copied();
        let mut step = match step_id {
            Some(id) => Some(tx.get_step(id).await?),
            None => None,
        };
        let mut payload = json!({
            "outbox_id": item.id,
            "idem_key": item.idem_key,
            "target": item.target,
            "detail": detail,
        });
        // Which repo a PR is for (a multi-repo task's PR list, `multi_repo`).
        if let Some(repo) = item.payload.get("repo") {
            payload["repo"] = repo.clone();
        }
        // A run-less row (a failed trigger's notice) keeps only its status.
        if let Some(run_id) = item.run_id {
            let step_key = step.as_ref().map(|s| s.step_key.clone());
            tx.append_event(NewEvent::new(run_id, step_key, kind, payload))
                .await?;
        }
        let mut effects = Vec::new();
        if let Some(step) = step.as_mut()
            && status != OutboxStatus::Unknown
        {
            self.outstanding.remove(&item.idem_key);
            if status == OutboxStatus::Failed {
                self.outstanding.retain(|_, s| *s != step.id);
            }
            let done = !self.outstanding.values().any(|s| *s == step.id);
            if step.status == StepStatus::Running && (done || status == OutboxStatus::Failed) {
                let result = if status == OutboxStatus::Failed {
                    Err(format!("`{}` failed", item.target))
                } else {
                    Ok(())
                };
                // Every PR of the step is open: link them on each other.
                if let (Ok(()), Some(mod_name)) = (&result, item.target.strip_suffix(".open_pr")) {
                    let prs = multi_repo::task_repos(&mut tx, step.run_id).await?;
                    let rows =
                        multi_repo::link_comments(step.run_id, &step.step_key, mod_name, &prs);
                    if multi_repo::enqueue(&mut tx, rows).await? {
                        effects.push(Effect::WakeOutbox);
                    }
                }
                self.finish(&mut tx, step, result).await?;
                self.advance(&mut tx, step.run_id, &mut effects).await?;
            }
        }
        tx.commit().await?;
        self.apply(effects).await
    }

    /// `sun.done`: the same move as a host `DoneClaimed`.
    async fn done(&mut self, run_id: RunId, step_key: StepKey, summary: String) -> Result<()> {
        let mut tx = self.store.begin().await?;
        let mut step = state::latest(tx.steps_of_run(run_id).await?)
            .remove(&step_key)
            .ok_or_else(|| Error::NotFound(format!("step `{step_key}` of run {run_id}")))?;
        // The summary is the agent's own text, even when the stream's earlier
        // events are still on their way from the host.
        if !summary.trim().is_empty() {
            self.watchdog.saw_output(step.id);
        }
        let claim = SunEvent::DoneClaimed { summary };
        let next = state::after_agent_event(step.status, &claim)
            .ok_or_else(|| Error::InvalidInput(format!("step `{step_key}` is {}", step.status)))?;
        tx.append_event(NewEvent::agent(run_id, step_key, &claim)?)
            .await?;
        let mut tripped = Vec::new();
        if self
            .police(&mut tx, &mut step, &claim, &mut tripped)
            .await?
        {
            tx.commit().await?;
            return self.apply(tripped).await;
        }
        move_step(&mut tx, &mut step, next, None).await?;
        if let SunEvent::DoneClaimed { summary } = &claim {
            crate::conversation::reply(&mut tx, run_id, summary).await?;
        }
        // Agent steps have no inline gates; `gate` steps verify (see `gates`).
        let mut effects = Vec::new();
        self.finish_agent(&mut tx, &mut step, &mut effects).await?;
        self.advance(&mut tx, run_id, &mut effects).await?;
        tx.commit().await?;
        self.apply(effects).await
    }

    /// Re-runs the run's running gate steps that are not checking now.
    async fn recheck(&mut self, run_id: RunId) -> Result<()> {
        let mut tx = self.store.begin().await?;
        let mut effects = Vec::new();
        if tx.get_run(run_id).await?.status == RunStatus::Running {
            self.rechecks.insert(run_id);
            self.advance(&mut tx, run_id, &mut effects).await?;
        }
        tx.commit().await?;
        self.apply(effects).await
    }

    async fn on_wake(&mut self, step_id: StepId) -> Result<()> {
        let mut tx = self.store.begin().await?;
        let mut step = tx.get_step(step_id).await?;
        let mut effects = Vec::new();
        if step.status == StepStatus::Running {
            self.finish(&mut tx, &mut step, Ok(())).await?;
            self.advance(&mut tx, step.run_id, &mut effects).await?;
        } else if step.status == StepStatus::WaitingApproval {
            let run = tx.get_run(step.run_id).await?;
            let workflow = self.workflow(&mut tx, &run).await?;
            let timeout = workflow
                .steps
                .iter()
                .find(|d| d.id == step.step_key)
                .and_then(|d| match &d.kind {
                    StepKind::Approval { timeout, .. } => *timeout,
                    _ => None,
                });
            match timeout.map(|t| (t, approval::on_timeout(&t))) {
                Some((t, OnTimeout::Remind)) => {
                    let payload = json!({ "step_id": step.id });
                    append(&mut tx, &step, kind::APPROVAL_REMINDER, payload).await?;
                    let after = Duration::from_secs(t.after_secs.max(1));
                    effects.push(Effect::Timer { step_id, after });
                }
                Some((_, OnTimeout::Reject(why))) => {
                    move_step(&mut tx, &mut step, StepStatus::Rejected, Some(why)).await?;
                    let payload = json!({ "approved": false, "by": "timeout" });
                    append(&mut tx, &step, kind::APPROVAL, payload).await?;
                    self.advance(&mut tx, step.run_id, &mut effects).await?;
                }
                None => {}
            }
        } else if step.status == StepStatus::LimitHit {
            self.retry_limit(&mut tx, &mut step, &mut effects).await?;
        } else if step.status == StepStatus::HandingOff {
            // A hand-off that waited for the marketplace (`failover`).
            if let Some(handoff) = self.pending_handoff(&mut tx, &step).await? {
                effects.push(Effect::Handoff(handoff));
            }
        } else if step.status == StepStatus::Failed {
            self.retry_due(&mut tx, &step, &mut effects).await?;
        }
        tx.commit().await?;
        self.apply(effects).await
    }

    /// Records a gate step's results and moves it (see `gates`).
    async fn on_gated(
        &mut self,
        step_id: StepId,
        results: Vec<GateResult>,
        scope: Option<Settled>,
    ) -> Result<()> {
        self.gating.remove(&step_id);
        // Blobs before the transaction (no I/O while a `Tx` is open).
        let mut files = Vec::new();
        for (name, text) in results.iter().filter_map(|r| r.file.as_ref()) {
            let blob = self.store.blobs().put(text.clone().into_bytes()).await?;
            files.push((name, blob));
        }
        let mut tx = self.store.begin().await?;
        let mut step = tx.get_step(step_id).await?;
        let mut effects = Vec::new();
        // Anything else: the run ended (or the step was cancelled) meanwhile.
        if step.status == StepStatus::Running {
            let run = tx.get_run(step.run_id).await?;
            for (name, blob) in &files {
                tx.add_artifact(run.task_id, name, blob, gates::mime_of(name))
                    .await?;
            }
            // `merge_order` waits for more merges: journal where it stands,
            // stay running, check again on the next `pr_merged`.
            if results.iter().any(|r| r.waiting) && results.iter().all(|r| r.pass || r.waiting) {
                for result in &results {
                    let payload =
                        json!({ "step_id": step.id, "attempt": step.attempt, "result": result });
                    append(&mut tx, &step, "gate_result", payload).await?;
                }
                tx.commit().await?;
                // A merge landed while this check ran on older PRs.
                if self.rechecks.contains(&step.run_id) {
                    return self.recheck(step.run_id).await;
                }
                return Ok(());
            }
            let workflow = self.workflow(&mut tx, &run).await?;
            let latest = state::latest(tx.steps_of_run(run.id).await?);
            move_step(&mut tx, &mut step, StepStatus::Verifying, None).await?;
            let verdict = gates::judge(&mut tx, &workflow, &latest, &step, &results).await?;
            corrections::on_gated(&mut tx, &step, &results).await?;
            let (next, why) = match verdict {
                Verdict::Pass => (StepStatus::Succeeded, None),
                Verdict::SendBack(why) => (StepStatus::Running, Some(why)),
                Verdict::Fail(why) => (StepStatus::Failed, Some(why)),
            };
            move_step(&mut tx, &mut step, next, why.as_deref()).await?;
            // The scope flag only asks for review: the verdict above stands.
            if let Some(scope) = scope.filter(|_| next == StepStatus::Succeeded) {
                scope.journal(&mut tx, run.id, Some(&step.step_key)).await?;
                let flag = sun_workflow::DecisionUse::Flag;
                self.act(&mut tx, &step, flag, &scope.answer, &mut effects)
                    .await?;
            }
            self.advance(&mut tx, run.id, &mut effects).await?;
        }
        tx.commit().await?;
        self.apply(effects).await
    }

    /// Records a device verify and moves its step (see `device`).
    async fn on_device(&mut self, step_id: StepId, result: Result<DeviceResult>) -> Result<()> {
        self.gating.remove(&step_id);
        let verified = match result {
            Ok(result) => Ok(device::store(self.store.blobs(), result).await?),
            Err(e) => Err(e),
        };
        let mut tx = self.store.begin().await?;
        let mut step = tx.get_step(step_id).await?;
        let mut effects = Vec::new();
        // Anything else: the run ended (or the step was cancelled) meanwhile.
        if step.status == StepStatus::Running {
            let run = tx.get_run(step.run_id).await?;
            let workflow = self.workflow(&mut tx, &run).await?;
            let with = workflow
                .steps
                .iter()
                .find(|d| d.id == step.step_key)
                .and_then(|d| match &d.kind {
                    StepKind::Extension { with, .. } => device::With::parse(with.as_ref()).ok(),
                    _ => None,
                })
                .ok_or_else(|| Error::NotFound(format!("device step `{}`", step.step_key)))?;
            let latest = state::latest(tx.steps_of_run(run.id).await?);
            let (verdict, enqueued) =
                device::record(&mut tx, (&workflow, &latest), &run, &step, &with, verified).await?;
            if enqueued {
                effects.push(Effect::WakeOutbox);
            }
            match verdict {
                Verdict::Pass => self.finish(&mut tx, &mut step, Ok(())).await?,
                Verdict::Fail(why) => self.finish(&mut tx, &mut step, Err(why)).await?,
                Verdict::SendBack(why) => {
                    move_step(&mut tx, &mut step, StepStatus::Verifying, None).await?;
                    move_step(&mut tx, &mut step, StepStatus::Running, Some(&why)).await?;
                }
            }
            self.advance(&mut tx, run.id, &mut effects).await?;
        }
        tx.commit().await?;
        self.apply(effects).await
    }

    /// Journals an extension step's mod answer (`extension_result`): its
    /// output succeeds the step, a mod error fails it.
    async fn on_extended(&mut self, step_id: StepId, result: Result<Value>) -> Result<()> {
        self.gating.remove(&step_id);
        let mut tx = self.store.begin().await?;
        let mut step = tx.get_step(step_id).await?;
        let mut effects = Vec::new();
        // Anything else: the run ended (or the step was cancelled) meanwhile.
        if step.status == StepStatus::Running {
            let result = result.map_err(|e| e.to_string());
            let payload = match &result {
                Ok(output) => json!({ "step_id": step.id, "output": output }),
                Err(error) => json!({ "step_id": step.id, "error": error }),
            };
            append(&mut tx, &step, kind::EXTENSION_RESULT, payload).await?;
            self.finish(&mut tx, &mut step, result.map(drop)).await?;
            self.advance(&mut tx, step.run_id, &mut effects).await?;
        }
        tx.commit().await?;
        self.apply(effects).await
    }

    /// The scheduler: create and start every ready step, then end the run if
    /// its steps decide it. Runs inside the caller's transaction.
    async fn advance(
        &mut self,
        tx: &mut Tx,
        run_id: RunId,
        effects: &mut Vec<Effect>,
    ) -> Result<()> {
        let run = tx.get_run(run_id).await?;
        if run.status != RunStatus::Running {
            return Ok(());
        }
        let workflow = self.workflow(tx, &run).await?;
        let mut latest = state::latest(tx.steps_of_run(run_id).await?);
        let mut again = false;
        for def in state::ready(&workflow, &latest) {
            let step = Step {
                id: StepId::generate(),
                run_id,
                step_key: def.id.clone(),
                attempt: 1,
                status: StepStatus::Queued,
                runtime_id: None,
                session_id: None,
                started_at: None,
                ended_at: None,
            };
            tx.insert_step(&step).await?;
            append(
                tx,
                &step,
                kind::STEP_STATUS,
                status_payload(&step, None, None),
            )
            .await?;
            latest.insert(step.step_key.clone(), step);
        }
        for def in &workflow.steps {
            // A sent-back step waits in `queued` until its `needs` succeed again.
            let needs_met = def.needs.iter().all(|key| {
                latest
                    .get(key)
                    .is_some_and(|s| s.status == StepStatus::Succeeded)
            });
            let Some(step) = latest.get_mut(&def.id).filter(|_| needs_met) else {
                continue;
            };
            if step.status == StepStatus::Queued {
                self.start_step(tx, def, step, effects).await?;
                again |= step.status == StepStatus::Succeeded;
            }
            // A running gate step checks whenever its `needs` hold: on start,
            // after a send-back came back, and after a restart.
            if let StepKind::Gate { gates, .. } = &def.kind
                && step.status == StepStatus::Running
                && self.gating.insert(step.id)
            {
                let worktree = worktree_of(tx, run.task_id).await?;
                let mut gates = gates.clone();
                for extra in decision::added_checks(tx, run_id).await? {
                    if !gates.iter().any(|g| g.name == extra.name) {
                        gates.push(extra);
                    }
                }
                match gates::Job::new(self.host(run_id), worktree, run_id, &gates) {
                    Ok(mut job) => {
                        if let Some(mod_name) = job.merge_order_mod().map(str::to_owned) {
                            let prs = multi_repo::task_repos(tx, run_id).await?;
                            self.rechecks.remove(&run_id);
                            // Same transaction as the check it reflects.
                            let rows =
                                multi_repo::statuses(run_id, &step.step_key, &mod_name, &prs);
                            if multi_repo::enqueue(tx, rows).await? {
                                effects.push(Effect::WakeOutbox);
                            }
                            job = job.with_prs(prs);
                        }
                        if job.needs_sources() {
                            job = job.with_sources(gates::content::run_sources(tx, run_id).await?);
                        }
                        let scope = match self.decisions.get() {
                            Some(client) => {
                                let task = tx.get_task(run.task_id).await?;
                                Some((decision::scope_hook(&task), client))
                            }
                            None => None,
                        };
                        effects.push(Effect::Gate {
                            step_id: step.id,
                            job,
                            scope,
                        })
                    }
                    Err(why) => {
                        self.gating.remove(&step.id);
                        self.finish(tx, step, Err(why)).await?;
                    }
                }
            }
            // A running device step verifies on start and after a restart.
            if let StepKind::Extension { name, with } = &def.kind
                && name == device::STEP_TYPE
                && step.status == StepStatus::Running
                && self.gating.insert(step.id)
            {
                let job = match device::With::parse(with.as_ref()) {
                    Ok(with) => {
                        let worktree = worktree_of(tx, run.task_id).await?;
                        device::Job::place(tx, worktree, with).await?
                    }
                    Err(why) => Err(why),
                };
                match job {
                    Ok(job) => effects.push(Effect::Device {
                        step_id: step.id,
                        job,
                    }),
                    Err(why) => {
                        self.gating.remove(&step.id);
                        self.finish(tx, step, Err(why)).await?;
                    }
                }
            }
            // Any other running extension step calls the mod named by its
            // type (`<mod>.<step type>`) on start and after a restart; the
            // idem key lets the mod answer a repeat without redoing it.
            if let StepKind::Extension { name, with } = &def.kind
                && name != device::STEP_TYPE
                && step.status == StepStatus::Running
                && self.gating.insert(step.id)
            {
                let (mod_name, method) = name.split_once('.').unwrap_or((name, ""));
                let action = format!("{name}#{}", step.attempt);
                let task = tx.get_task(run.task_id).await?;
                let call = sun_core::ModCall {
                    mod_name: mod_name.to_owned(),
                    method: method.to_owned(),
                    params: json!({
                        "task_id": run.task_id,
                        "run_id": run_id,
                        "step_key": step.step_key,
                        "branch": tasks::branch_of(tx, &task).await?,
                        "with": with,
                    }),
                    idem_key: Some(IdemKey::new(run_id, &step.step_key, &action)),
                };
                effects.push(Effect::Extension {
                    step_id: step.id,
                    call,
                });
            }
            // A running decide step asks on start and after a restart.
            if let StepKind::Decide { question, .. } = &def.kind
                && step.status == StepStatus::Running
                && self.gating.insert(step.id)
            {
                self.ask_decision(tx, run.task_id, question, step, effects)
                    .await?;
            }
        }
        // A step finished on the spot: its dependents are ready now.
        if again {
            return Box::pin(self.advance(tx, run_id, effects)).await;
        }
        // A failed step its `retry:` policy will retry doesn't end the run.
        let retrying = self
            .schedule_retries(tx, &workflow, &latest, effects)
            .await?;
        let outcome = match retrying.is_empty() {
            true => state::run_outcome(&workflow, &latest),
            false => {
                let mut view = latest.clone();
                view.retain(|key, _| !retrying.contains(key));
                state::run_outcome(&workflow, &view)
            }
        };
        match outcome {
            RunOutcome::Ongoing => Ok(()),
            RunOutcome::Succeeded => {
                let unverified = gates::unverified(&workflow);
                let suggest = match unverified {
                    Some(_) => Some((run_id, worktree_of(tx, run.task_id).await?)),
                    None => None,
                };
                let removal = plan_removal(tx, self.host(run_id), run.task_id).await?;
                effects.push(Effect::RemoveWorktree { removal, suggest });
                move_run(tx, run_id, RunStatus::Succeeded, unverified).await
            }
            RunOutcome::Ended(status, reason) => {
                // A `stalled` step that ended the run keeps its state.
                self.end_run(tx, run_id, &mut latest, status, &reason, true, effects)
                    .await
            }
        }
    }

    /// Cancels the run's open steps (`stalled` ones too unless
    /// `keep_stalled`), stops their sessions and moves the run to the
    /// terminal `status`.
    #[allow(clippy::too_many_arguments)]
    async fn end_run(
        &mut self,
        tx: &mut Tx,
        run_id: RunId,
        latest: &mut HashMap<StepKey, Step>,
        status: RunStatus,
        reason: &str,
        keep_stalled: bool,
        effects: &mut Vec<Effect>,
    ) -> Result<()> {
        let open = |s: &&mut Step| {
            let kept = keep_stalled && s.status == StepStatus::Stalled;
            !(s.status.is_terminal() || kept)
        };
        for step in latest.values_mut().filter(open) {
            move_step(tx, step, StepStatus::Cancelled, Some("run ended")).await?;
            self.outstanding.retain(|_, s| *s != step.id);
            // The agent's `sun.ask` returns `Unavailable`.
            self.questions.retain(|_, (s, _)| *s != step.id);
            self.stop_step_sessions(step.id, effects);
        }
        // A failed run keeps its worktrees for a retry; a cancelled one is over.
        if status == RunStatus::Cancelled {
            let task_id = tx.get_run(run_id).await?.task_id;
            let removal = plan_removal(tx, self.host(run_id), task_id).await?;
            effects.push(Effect::RemoveWorktree {
                removal,
                suggest: None,
            });
        }
        move_run(tx, run_id, status, Some(reason)).await
    }

    /// Enqueues a running action/notify step's outbox rows and waits for
    /// them; with nothing new to deliver the step succeeds. `changed`: only
    /// these repos (slugs) of a multi-repo task get a PR.
    async fn enqueue_side_effects(
        &mut self,
        tx: &mut Tx,
        def: &StepDef,
        step: &mut Step,
        effects: &mut Vec<Effect>,
        changed: Option<&[String]>,
    ) -> Result<()> {
        let mut waiting = false;
        let mut facts = outbox::RunFacts::load(tx, step.run_id).await?;
        if let Some(changed) = changed {
            facts
                .repos
                .retain(|r| changed.contains(&outbox::repo_slug(&r.url)));
        }
        // A retried step sends its failed rows again, under the same keys.
        let failed: HashMap<IdemKey, sun_core::OutboxId> = match step.attempt {
            1 => HashMap::new(),
            _ => tx
                .outbox_with_status(OutboxStatus::Failed, u32::MAX)
                .await?
                .into_iter()
                .filter(|row| row.run_id == Some(step.run_id))
                .map(|row| (row.idem_key, row.id))
                .collect(),
        };
        for item in outbox::side_effects(&facts, def) {
            // `false`: an earlier attempt already enqueued this key;
            // that row stands and isn't awaited again, unless it failed.
            let again = failed.get(&item.idem_key);
            if let Some(id) = again {
                tx.set_outbox_status(*id, OutboxStatus::Pending, None)
                    .await?;
            }
            if again.is_some() || tx.enqueue(&item).await? {
                self.outstanding.insert(item.idem_key, step.id);
                waiting = true;
            }
        }
        if waiting {
            effects.push(Effect::WakeOutbox);
        } else {
            self.finish(tx, step, Ok(())).await?;
        }
        Ok(())
    }

    /// A git host's `open_pr` on a task with a repo pushes the task branch
    /// first (`push`); its outbox row is enqueued once the push succeeded
    /// ([`Self::on_pushed`]). An `.upload` on a repo-less task snapshots the
    /// task folder first ([`Self::on_snapshot`]). True when either was
    /// scheduled.
    async fn push_first(
        &mut self,
        tx: &mut Tx,
        def: &StepDef,
        step: &Step,
        effects: &mut Vec<Effect>,
    ) -> Result<bool> {
        let StepKind::Action { target, .. } = &def.kind else {
            return Ok(false);
        };
        let host = self.host(step.run_id);
        if target.ends_with(outbox::UPLOAD) {
            let task_id = tx.get_run(step.run_id).await?.task_id;
            let worktree = worktree_of(tx, task_id).await?;
            if !matches!(worktree, Worktree::Folder { .. }) {
                return Ok(false);
            }
            if self.gating.insert(step.id) {
                let job = crate::snapshot::Job { host, worktree };
                effects.push(Effect::Snapshot {
                    step_id: step.id,
                    job,
                });
            }
            return Ok(true);
        }
        let Some(mod_name) = target.strip_suffix(".open_pr") else {
            return Ok(false);
        };
        let facts = outbox::RunFacts::load(tx, step.run_id).await?;
        let worktree = worktree_of(tx, facts.task.id).await?;
        let Some(job) = push::Job::new(tx, host, &facts, worktree, mod_name, &self.vcs).await?
        else {
            return Ok(false);
        };
        if self.gating.insert(step.id) {
            effects.push(Effect::Push {
                step_id: step.id,
                job,
            });
        }
        Ok(true)
    }

    /// A task-folder snapshot finished: its files become artifact versions.
    /// An `.upload` action waiting on it enqueues its delivery (or fails).
    async fn on_snapshot(
        &mut self,
        step_id: StepId,
        result: std::result::Result<Vec<(String, Vec<u8>)>, String>,
    ) -> Result<()> {
        // Blobs before the transaction (no I/O while a `Tx` is open).
        let mut files = Vec::new();
        for (name, bytes) in result.iter().flatten() {
            files.push((name, self.store.blobs().put(bytes.clone()).await?));
        }
        let mut tx = self.store.begin().await?;
        let mut step = tx.get_step(step_id).await?;
        let run = tx.get_run(step.run_id).await?;
        for (name, blob) in &files {
            tx.add_artifact(run.task_id, name, blob, gates::mime_of(name))
                .await?;
        }
        let mut effects = Vec::new();
        if self.gating.remove(&step_id) && step.status == StepStatus::Running {
            let workflow = self.workflow(&mut tx, &run).await?;
            let def = workflow.steps.iter().find(|d| d.id == step.step_key);
            match (result, def) {
                (Ok(_), Some(def)) => {
                    self.enqueue_side_effects(&mut tx, def, &mut step, &mut effects, None)
                        .await?;
                }
                (Ok(_), None) => {}
                (Err(why), _) => self.finish(&mut tx, &mut step, Err(why)).await?,
            }
            self.advance(&mut tx, run.id, &mut effects).await?;
        }
        tx.commit().await?;
        self.apply(effects).await
    }

    /// A push before `open_pr` finished: enqueue the PR, or fail the step.
    async fn on_pushed(
        &mut self,
        step_id: StepId,
        result: std::result::Result<Option<Vec<String>>, String>,
    ) -> Result<()> {
        self.gating.remove(&step_id);
        let mut tx = self.store.begin().await?;
        let mut step = tx.get_step(step_id).await?;
        let mut effects = Vec::new();
        // Anything else: the run ended (or the step was cancelled) meanwhile.
        if step.status == StepStatus::Running {
            let run = tx.get_run(step.run_id).await?;
            let workflow = self.workflow(&mut tx, &run).await?;
            let def = workflow.steps.iter().find(|d| d.id == step.step_key);
            match (result, def) {
                (Ok(changed), Some(def)) => {
                    self.enqueue_side_effects(
                        &mut tx,
                        def,
                        &mut step,
                        &mut effects,
                        changed.as_deref(),
                    )
                    .await?;
                }
                (Ok(_), None) => {}
                (Err(why), _) => self.finish(&mut tx, &mut step, Err(why)).await?,
            }
            self.advance(&mut tx, run.id, &mut effects).await?;
        }
        tx.commit().await?;
        self.apply(effects).await
    }

    /// **Step drivers**: how each step type leaves `queued`. Agent steps go
    /// to a host; every other type is driven by the engine itself
    /// (`queued → running`).
    async fn start_step(
        &mut self,
        tx: &mut Tx,
        def: &StepDef,
        step: &mut Step,
        effects: &mut Vec<Effect>,
    ) -> Result<()> {
        match &def.kind {
            StepKind::Agent { .. } => {
                // The ecosystem's monthly cap is reached: no new spending.
                if let Some(why) = crate::budget::ecosystem_over(tx, chrono::Utc::now()).await? {
                    move_step(tx, step, StepStatus::Running, None).await?;
                    return move_step(tx, step, StepStatus::BudgetExceeded, Some(&why)).await;
                }
                move_step(tx, step, StepStatus::Dispatched, None).await?;
                let adapter = self.first_runtime(tx, step).await?;
                let prompt = self.agent_prompt(tx, step).await?;
                let (prompt, resume) = self.follow_up(tx, step, &adapter, prompt).await?;
                self.assign(tx, step, &adapter, &prompt, None, resume, effects)
                    .await?;
            }
            StepKind::Action { .. } | StepKind::Notify { .. } => {
                move_step(tx, step, StepStatus::Running, None).await?;
                if !self.push_first(tx, def, step, effects).await? {
                    self.enqueue_side_effects(tx, def, step, effects, None)
                        .await?;
                }
            }
            StepKind::Wait { for_secs } => {
                move_step(tx, step, StepStatus::Running, None).await?;
                effects.push(Effect::Timer {
                    step_id: step.id,
                    after: Duration::from_secs(*for_secs),
                });
            }
            StepKind::Approval { timeout, when, .. } => {
                move_step(tx, step, StepStatus::Running, None).await?;
                if let Some(sun_workflow::ApprovalWhen::DataTouched) = when
                    && !crate::data_touched::touched(tx, step.run_id).await?
                {
                    move_step(tx, step, StepStatus::Verifying, None).await?;
                    let why = "not needed: no data touched (detected)";
                    return move_step(tx, step, StepStatus::Succeeded, Some(why)).await;
                }
                move_step(tx, step, StepStatus::WaitingApproval, None).await?;
                if let Some(timeout) = timeout {
                    let after = Duration::from_secs(timeout.after_secs.max(1));
                    effects.push(Effect::Timer {
                        step_id: step.id,
                        after,
                    });
                }
            }
            // Its checks start in `advance` once the step is running.
            StepKind::Gate { .. } | StepKind::Decide { .. } => {
                move_step(tx, step, StepStatus::Running, None).await?
            }
            // Its verify starts in `advance` once the step is running.
            StepKind::Extension { name, .. } if name == device::STEP_TYPE => {
                move_step(tx, step, StepStatus::Running, None).await?;
            }
            // Its mod call starts in `advance` once the step is running.
            StepKind::Extension { .. } => {
                move_step(tx, step, StepStatus::Running, None).await?;
            }
        }
        Ok(())
    }

    /// Picks a new session id of runtime `adapter` for a `dispatched` step,
    /// journals it, and schedules the host call for after commit.
    /// `runtime` is a chain name; its `runtime` row (created with
    /// `adapter = name` when missing) says which adapter the host runs.
    async fn assign_session(
        &mut self,
        tx: &mut Tx,
        step: &Step,
        runtime: &str,
        prompt: &str,
        handoff: Option<sun_context::HandoffPack>,
        effects: &mut Vec<Effect>,
    ) -> Result<SessionId> {
        self.assign(tx, step, runtime, prompt, handoff, None, effects)
            .await
    }

    /// [`Self::assign_session`] that may first resume the CLI's own session.
    #[allow(clippy::too_many_arguments)]
    async fn assign(
        &mut self,
        tx: &mut Tx,
        step: &Step,
        runtime: &str,
        prompt: &str,
        handoff: Option<sun_context::HandoffPack>,
        resume: Option<dispatch::Resume>,
        effects: &mut Vec<Effect>,
    ) -> Result<SessionId> {
        let session_id = SessionId::generate();
        // A chain name means the task's workspace's runtime of that name.
        let workspace = tx.workspace_of_run(step.run_id).await?;
        let row = tx
            .upsert_runtime(workspace, runtime, runtime, RuntimeSource::ByoRuntime)
            .await?;
        tx.insert_session(session_id, self.host(step.run_id))
            .await?;
        tx.assign_step_session(step.id, row.id, session_id).await?;
        let exec_env = self.exec_env(tx, step.run_id).await?;
        let (model, effort, effort_ignored) =
            self.entry_settings(tx, step, runtime, &row.adapter).await?;
        let run = tx.get_run(step.run_id).await?;
        let workflow = self.workflow(tx, &run).await?;
        let step_mode =
            crate::agents::step_mode(tx, run.workflow_id, &workflow, &step.step_key).await?;
        let permission_mode = step_mode.map(|m| m.as_str().to_owned());
        append(
            tx,
            step,
            kind::SESSION_ASSIGNED,
            // The window and threshold feed the UI's context meter.
            json!({
                "step_id": step.id,
                "session_id": session_id,
                "adapter": runtime,
                // Who pays for this session.
                "source": row.source,
                "exec_env": exec_env,
                "context_window": self.context_window(&row.adapter),
                "rotate_at_percent": self.config.watchdog.rotate_at_percent,
                "model": model,
                "effort": effort,
                "effort_ignored": effort_ignored,
            }),
        )
        .await?;
        let mut prompt = gates::with_feedback(tx, step, prompt).await?;
        // A fresh step (not a hand-off or resume) takes the queued messages.
        if handoff.is_none() && resume.is_none() {
            prompt = crate::queued::take_queued(tx, step, prompt).await?;
        }
        effects.push(Effect::Start(Start {
            step: step.clone(),
            session_id,
            adapter: row.adapter,
            prompt,
            handoff: handoff.map(Box::new),
            resume,
            exec_env,
            byo_key: row.byo_key,
            model,
            effort,
            permission_mode,
        }));
        Ok(session_id)
    }

    /// The model and effort chain entry `runtime` of the step's run sets.
    /// The effort goes to the CLI only when the run's host lists it for
    /// `adapter` and that model (its last report); else it is returned as
    /// ignored, so the journal says why the CLI's default ran.
    async fn entry_settings(
        &mut self,
        tx: &mut Tx,
        step: &Step,
        runtime: &str,
        adapter: &str,
    ) -> Result<(Option<String>, Option<String>, Option<String>)> {
        let run = tx.get_run(step.run_id).await?;
        let entries = failover::entries(&*self.workflow(tx, &run).await?, &self.config.adapter);
        let Some(entry) = failover::entry(&entries, runtime) else {
            return Ok((None, None, None));
        };
        let model = entry.model().map(str::to_owned);
        let Some(effort) = entry.effort() else {
            return Ok((model, None, None));
        };
        let offered = match tx.get_host(self.host(step.run_id)).await {
            Ok(host) => host
                .effort_catalog(adapter)
                .allows(model.as_deref(), effort),
            Err(_) => false,
        };
        Ok(match offered {
            true => (model, Some(effort.to_owned()), None),
            false => (model, None, Some(effort.to_owned())),
        })
    }

    /// Forgets the sessions of `step_id` and stops them after commit.
    fn stop_step_sessions(&mut self, step_id: StepId, effects: &mut Vec<Effect>) {
        let ours: Vec<SessionId> = self
            .sessions
            .iter()
            .filter(|(_, (s, _))| *s == step_id)
            .map(|(id, _)| *id)
            .collect();
        for id in ours {
            if let Some((_, handle)) = self.forget_session(id) {
                effects.push(Effect::Stop(handle));
            }
        }
    }

    /// Drops a session from routing and revokes its `sun` MCP token.
    fn forget_session(&mut self, id: SessionId) -> Option<(StepId, SessionHandle)> {
        if let (Some(mcp), Some(token)) = (&self.mcp, self.mcp_tokens.remove(&id)) {
            mcp.sessions.revoke(&token);
        }
        self.rotations.remove(&id);
        self.rotation_guard.born_full.remove(&id);
        self.sessions.remove(&id)
    }

    /// The engine's verdict on a `running` or `verifying` step: `succeeded`
    /// or `failed` (through `verifying`). Only the engine calls this.
    async fn finish(
        &mut self,
        tx: &mut Tx,
        step: &mut Step,
        result: std::result::Result<(), String>,
    ) -> Result<()> {
        if step.status == StepStatus::Running {
            move_step(tx, step, StepStatus::Verifying, None).await?;
        }
        match result {
            Ok(()) => move_step(tx, step, StepStatus::Succeeded, None).await,
            Err(why) => move_step(tx, step, StepStatus::Failed, Some(&why)).await,
        }
    }

    /// An agent step's done claim held: it succeeds, and the canvas pins
    /// that were with the agent are done, in the same transaction.
    /// A step that failed over may wait for a human review first.
    /// A repo-less task's folder is snapshotted into artifacts.
    async fn finish_agent(
        &mut self,
        tx: &mut Tx,
        step: &mut Step,
        effects: &mut Vec<Effect>,
    ) -> Result<()> {
        if self.review_after_failover(tx, step).await? {
            return Ok(());
        }
        self.finish(tx, step, Ok(())).await?;
        let task_id = tx.get_run(step.run_id).await?.task_id;
        let worktree = worktree_of(tx, task_id).await?;
        if matches!(worktree, Worktree::Folder { .. }) {
            let host = self.host(step.run_id);
            effects.push(Effect::Snapshot {
                step_id: step.id,
                job: crate::snapshot::Job { host, worktree },
            });
        }
        let done = crate::pins::agent_done(tx, task_id).await?;
        // The crop waits on the host's browser (seconds), then writes in its
        // own transaction after this one.
        let (store, hosts) = (&self.store, &self.hosts);
        crate::pins::spawn_capture(
            store,
            hosts,
            self.config.host,
            task_id,
            done,
            PreviewSide::After,
        );
        Ok(())
    }

    async fn apply(&mut self, effects: Vec<Effect>) -> Result<()> {
        for effect in effects {
            match effect {
                Effect::Start(start) => Box::pin(self.start_session(start)).await?,
                Effect::Handoff(handoff) => self.hand_off(handoff).await?,
                Effect::Stop(handle) => {
                    // Best effort: the step is already cancelled in the store.
                    let _ = self.hosts.stop_session(&handle, StopMode::Kill).await;
                }
                Effect::Timer { step_id, after } => {
                    let commands = self.commands.clone();
                    tokio::spawn(async move {
                        tokio::time::sleep(after).await;
                        let _ = commands.send(Command::Wake(step_id)).await;
                    });
                }
                Effect::WakeOutbox => self.wake.notify_one(),
                Effect::RemoveWorktree { removal, suggest } => {
                    let (hosts, store) = (self.hosts.clone(), self.store.clone());
                    tokio::spawn(async move {
                        let host = removal.host;
                        if let Some((run_id, worktree)) = suggest
                            && let Err(e) =
                                gates::suggest(&*hosts, &store, host, run_id, &worktree).await
                        {
                            eprintln!("sun: suggesting a gate for run {run_id}: {e}");
                        }
                        let wait = (removal.due_at - chrono::Utc::now()).to_std();
                        tokio::time::sleep(wait.unwrap_or_default()).await;
                        if let Err(e) = remove_worktrees(&*hosts, &store, &removal).await {
                            let task_id = removal.task_id;
                            eprintln!("sun: removing the worktrees of task {task_id}: {e}");
                        }
                    });
                }
                Effect::Gate {
                    step_id,
                    job,
                    scope,
                } => {
                    let (hosts, mods) = (self.hosts.clone(), self.mods.clone());
                    let commands = self.commands.clone();
                    tokio::spawn(async move {
                        let probe = scope.as_ref().map(|_| job.clone());
                        let results = job.run(&*hosts, &*mods).await;
                        let passed = results.iter().all(|r| r.pass);
                        let scope = match (scope, probe) {
                            (Some((mut hook, client)), Some(probe)) if passed => {
                                hook.state["changes"] = json!(probe.changes(&*hosts).await);
                                Some(hook.ask(Some(client)).await)
                            }
                            _ => None,
                        };
                        let gated = Command::Gated {
                            step_id,
                            results,
                            scope,
                        };
                        let _ = commands.send(gated).await;
                    });
                }
                Effect::Ask {
                    step_id,
                    hook,
                    client,
                } => {
                    let commands = self.commands.clone();
                    tokio::spawn(async move {
                        let decides = hook.decides;
                        let settled = hook.ask(Some(client)).await;
                        let hooked = Command::Hooked {
                            step_id,
                            decides,
                            settled,
                        };
                        let _ = commands.send(hooked).await;
                    });
                }
                // Read before the next effect starts the next step's session,
                // so the snapshot holds this step's output.
                // ponytail: the engine waits for the host's reads, as it does
                // for a session start; spawn it if task folders get big.
                Effect::Snapshot { step_id, job } => {
                    let result = job.run(&*self.hosts).await;
                    let commands = self.commands.clone();
                    tokio::spawn(async move {
                        let _ = commands
                            .send(Command::Snapshotted { step_id, result })
                            .await;
                    });
                }
                Effect::Device { step_id, job } => {
                    let (hosts, commands) = (self.hosts.clone(), self.commands.clone());
                    tokio::spawn(async move {
                        let result = job.run(&*hosts).await;
                        let _ = commands.send(Command::Device { step_id, result }).await;
                    });
                }
                Effect::Extension { step_id, call } => {
                    let (mods, commands) = (self.mods.clone(), self.commands.clone());
                    tokio::spawn(async move {
                        let result = mods.call(call).await;
                        let _ = commands.send(Command::Extended { step_id, result }).await;
                    });
                }
                Effect::Push { step_id, job } => {
                    let (hosts, commands) = (self.hosts.clone(), self.commands.clone());
                    tokio::spawn(async move {
                        let result = job.run(&*hosts).await;
                        let _ = commands.send(Command::Pushed { step_id, result }).await;
                    });
                }
                Effect::Decide { step_id, job } => {
                    let commands = self.commands.clone();
                    tokio::spawn(async move {
                        let result = job.run().await;
                        let _ = commands.send(Command::Decided { step_id, result }).await;
                    });
                }
            }
        }
        Ok(())
    }

    /// Calls the host, then records `dispatched → running` with the handle.
    /// A crash in between leaves `dispatched` plus the assigned session id,
    /// which resume probes.
    // ponytail: the engine waits for the host's start reply; move the call to
    // a spawned task with a `Started` command if hosts get slow to answer.
    async fn start_session(&mut self, start: Start) -> Result<()> {
        let Start {
            ref step,
            session_id,
            ref adapter,
            ref prompt,
            ..
        } = start;
        let pending = SessionHandle {
            session_id,
            native_id: None,
            pid: None,
        };
        self.sessions.insert(session_id, (step.id, pending));
        let mut delivered = None;
        let mut path = String::new();
        let started = match self.worktree(step.run_id).await {
            Ok(worktree) => 'start: {
                path = worktree.display().to_string();
                // Behind placement and failover: a bypass session the operator
                // turned off never reaches the host, failed like its own refusal.
                let mode = start.permission_mode.as_deref();
                if let Some(why) = failover::bypass_disabled(mode, self.config.allow_bypass) {
                    break 'start Err(Error::Forbidden(why));
                }
                let handoff = start.handoff.as_deref().cloned();
                let host = self.host(step.run_id);
                let mcp_file = match &self.mcp {
                    Some(mcp) => {
                        let grants =
                            crate::mcp::session_grants(&self.store, step.run_id, &step.step_key)
                                .await?;
                        let token = mcp.sessions.issue(step.run_id, step.step_key.clone());
                        let file = mcp.file(&token, host, &grants);
                        self.mcp_tokens.insert(session_id, token);
                        Some(file?)
                    }
                    None => None,
                };
                let context = match self
                    .deliver_context(step, adapter, &worktree, prompt, handoff, mcp_file)
                    .await
                {
                    Ok(context) => context,
                    Err(e) => break 'start Err(e),
                };
                let envelope = context.envelope.clone();
                let envelope_parts = context.envelope_parts.clone();
                let mcp_config = context.mcp_config.clone();
                delivered = Some(context);
                let mut read = self.store.read().await?;
                // Only the skills the run's workspace installed; none in a
                // chat without an agent (`None` would sync every skill).
                let workspace = read.workspace_of_run(step.run_id).await?;
                let workflow = read.get_run(step.run_id).await?.workflow_id;
                let skills = Some(match read.workflow_owner(workflow).await? {
                    WorkflowOwner::DirectChat => Vec::new(),
                    _ => read.catalog_installs(workspace).await?,
                });
                drop(read);
                let spec = SessionSpec {
                    skills,
                    permission_mode: start.permission_mode.clone(),
                    session_id,
                    run_id: step.run_id,
                    step_key: step.step_key.clone(),
                    worktree,
                    envelope,
                    envelope_parts,
                    model: start.model.clone(),
                    effort: start.effort.clone(),
                    mcp_config,
                    env: BTreeMap::new(),
                    exec_env: start.exec_env.clone(),
                    resume: start.resume.as_ref().map(|r| r.native_id.clone()),
                    byo_key: start.byo_key.clone(),
                };
                match with_byo_key(&self.store, spec).await {
                    Ok(spec) => {
                        self.hosts
                            .start_session(self.host(step.run_id), adapter, spec)
                            .await
                    }
                    Err(e) => Err(e),
                }
            }
            Err(e) => Err(e),
        };
        let mut tx = self.store.begin().await?;
        let mut current = tx.get_step(step.id).await?;
        let mut orphan = None;
        let mut effects = Vec::new();
        match started {
            Ok(handle) if current.status == StepStatus::Dispatched => {
                self.started(&start, &handle);
                self.sessions.insert(session_id, (step.id, handle.clone()));
                tx.set_session_started(&handle, &path).await?;
                move_step(&mut tx, &mut current, StepStatus::Running, None).await?;
                append(
                    &mut tx,
                    &current,
                    kind::SESSION_STARTED,
                    json!({ "step_id": step.id, "session": handle }),
                )
                .await?;
                if let Some(context) = delivered {
                    tx.insert_context_pack(&context.row).await?;
                    let payload = json!({
                        "step_id": step.id,
                        "context_pack_id": context.row.id,
                        "summary": context.summary,
                    });
                    append(&mut tx, &current, kind::CONTEXT_PACK, payload).await?;
                }
                self.watch(&mut tx, &current).await?;
            }
            // The step moved on (e.g. cancelled) while the host was starting.
            Ok(handle) => orphan = Some(handle),
            // Stays `dispatched`; tried again with backoff (`dispatch`).
            Err(e) => {
                self.forget_session(session_id);
                append(
                    &mut tx,
                    &current,
                    kind::DISPATCH_FAILED,
                    json!({ "step_id": step.id, "session_id": session_id, "error": e.to_string() }),
                )
                .await?;
                self.start_failed(&mut tx, &mut current, start, &e, &mut effects)
                    .await?;
            }
        }
        tx.commit().await?;
        if let Some(handle) = orphan {
            self.forget_session(session_id);
            let _ = self.hosts.stop_session(&handle, StopMode::Kill).await;
        }
        self.apply(effects).await
    }

    /// Prepares the run's task worktree on the host (idempotent: every
    /// session and gate of a task shares it) and returns its host path.
    /// The first time per run, journals each repo's base commit (`base`).
    async fn worktree(&mut self, run_id: RunId) -> Result<PathBuf> {
        let mut read = self.store.read().await?;
        let task_id = read.get_run(run_id).await?.task_id;
        let worktree = worktree_of(&mut read, task_id).await?;
        drop(read);
        let host = self.host(run_id);
        let path = self.hosts.prepare_worktree(host, worktree.clone()).await?;
        if self.based.insert(run_id) {
            let repos = crate::base::read(self.hosts.as_ref(), host, &worktree, &path).await;
            if !repos.is_empty() {
                let mut tx = self.store.begin().await?;
                let payload = json!({ "repos": repos });
                tx.append_event(NewEvent::new(run_id, None, crate::base::RUN_BASE, payload))
                    .await?;
                tx.commit().await?;
            }
        }
        Ok(path)
    }

    /// Resume on boot: for every `running` run, probe its agent
    /// sessions (alive → re-attach, gone → `interrupted → dispatched` on a
    /// new session), re-arm timers and outbox waits, then advance. Steps in
    /// `succeeded` are never touched. `queued` runs wait for the queue,
    /// `paused` runs for a human.
    async fn resume(&mut self) -> Result<()> {
        struct Active {
            run_id: RunId,
            workflow: Arc<Workflow>,
            steps: HashMap<StepKey, Step>,
            sessions: HashMap<StepId, SessionHandle>,
        }
        let mut read = self.store.read().await?;
        let mut open = HashSet::new();
        for status in [OutboxStatus::Pending, OutboxStatus::Unknown] {
            for item in read.outbox_with_status(status, u32::MAX).await? {
                open.insert(item.idem_key);
            }
        }
        let mut active = Vec::new();
        for run in read.active_runs().await? {
            if let Some(host) = queue::placed_host(&mut read, run.id).await? {
                self.placed.insert(run.id, host);
            }
            if run.status != RunStatus::Running {
                continue;
            }
            let steps = state::latest(read.steps_of_run(run.id).await?);
            active.push(Active {
                run_id: run.id,
                workflow: self.workflow(&mut read, &run).await?,
                sessions: sessions_of(&mut read, &steps).await?,
                steps,
            });
        }
        drop(read);

        for run in active {
            // Probe first: no write transaction is open during host I/O.
            let mut probes = HashMap::new();
            for step in run.steps.values() {
                if matches!(step.status, StepStatus::Dispatched | StepStatus::Running)
                    && let Some(handle) = run.sessions.get(&step.id)
                {
                    probes.insert(step.id, (handle.clone(), self.hosts.is_alive(handle).await));
                }
            }
            let mut tx = self.store.begin().await?;
            let mut effects = Vec::new();
            for def in &run.workflow.steps {
                let Some(mut step) = run.steps.get(&def.id).cloned() else {
                    continue;
                };
                match (&def.kind, step.status) {
                    (StepKind::Agent { .. }, StepStatus::Dispatched | StepStatus::Running) => {
                        let prompt = self.agent_prompt(&mut tx, &step).await?;
                        match probes.remove(&step.id) {
                            Some((handle, probe)) => {
                                self.on_probe(
                                    &mut tx,
                                    &mut step,
                                    &prompt,
                                    handle,
                                    probe,
                                    &mut effects,
                                )
                                .await?;
                            }
                            // Never got a session: start one.
                            None => {
                                let adapter = self.runtime_of(&mut tx, &step).await?;
                                self.assign_session(
                                    &mut tx,
                                    &step,
                                    &adapter,
                                    &prompt,
                                    None,
                                    &mut effects,
                                )
                                .await?;
                            }
                        }
                    }
                    (StepKind::Action { .. } | StepKind::Notify { .. }, StepStatus::Running) => {
                        let mut waiting = false;
                        let facts = outbox::RunFacts::load(&mut tx, run.run_id).await?;
                        for item in outbox::side_effects(&facts, def) {
                            if open.contains(&item.idem_key) {
                                self.outstanding.insert(item.idem_key, step.id);
                                waiting = true;
                            }
                        }
                        // Nothing open: delivered, or an `open_pr` whose push
                        // never finished (pushing again is a no-op).
                        if !waiting && !self.push_first(&mut tx, def, &step, &mut effects).await? {
                            self.finish(&mut tx, &mut step, Ok(())).await?;
                        }
                    }
                    (StepKind::Wait { for_secs }, StepStatus::Running) => {
                        let due = step
                            .started_at
                            .map(|at| {
                                at + chrono::Duration::seconds(
                                    i64::try_from(*for_secs).unwrap_or(i64::MAX),
                                )
                            })
                            .unwrap_or_else(chrono::Utc::now);
                        let after = (due - chrono::Utc::now()).to_std().unwrap_or_default();
                        effects.push(Effect::Timer {
                            step_id: step.id,
                            after,
                        });
                    }
                    (
                        StepKind::Approval {
                            timeout: Some(timeout),
                            ..
                        },
                        StepStatus::WaitingApproval,
                    ) => {
                        let now = chrono::Utc::now();
                        let after = approval::next_timeout(timeout, step.started_at, now);
                        effects.push(Effect::Timer {
                            step_id: step.id,
                            after,
                        });
                    }
                    // A crash can't leave an agent here today (done → succeeded
                    // is one transaction).
                    (StepKind::Agent { .. }, StepStatus::Verifying) => {
                        self.finish_agent(&mut tx, &mut step, &mut effects).await?;
                    }
                    // Failover: decide again, or redo a hand-off cut short.
                    (StepKind::Agent { .. }, StepStatus::LimitHit) => {
                        effects.push(Effect::Timer {
                            step_id: step.id,
                            after: Duration::ZERO,
                        });
                    }
                    (StepKind::Agent { .. }, StepStatus::HandingOff | StepStatus::Interrupted) => {
                        if let Some(handoff) = self.pending_handoff(&mut tx, &step).await? {
                            effects.push(Effect::Handoff(handoff));
                        }
                    }
                    // Running gate steps re-check in `advance`; other
                    // waiting_approval steps wait for `decide`; stalled
                    // belongs to the watchdog.
                    _ => {}
                }
            }
            self.advance(&mut tx, run.run_id, &mut effects).await?;
            tx.commit().await?;
            self.apply(effects).await?;
        }
        // Removals planned before the restart.
        let pending = pending_removals(&mut self.store.read().await?).await?;
        let effects = pending.into_iter().map(|removal| Effect::RemoveWorktree {
            removal,
            suggest: None,
        });
        self.apply(effects.collect()).await
    }

    /// Where the run's agent sessions run: the task's override
    /// ([`task_exec_env_key`]), else the workflow's `exec_env:` with the
    /// image or template the repo declares ([`repo_exec_env_key`]).
    async fn exec_env(&mut self, tx: &mut Tx, run_id: RunId) -> Result<ExecEnvSpec> {
        let run = tx.get_run(run_id).await?;
        // `null`: none (a released lease with no override to put back).
        let own = tx.setting(&task_exec_env_key(run.task_id)).await?;
        if let Some(spec) = own.filter(|v| !v.is_null()) {
            return serde_json::from_value(spec).map_err(|e| Error::InvalidInput(e.to_string()));
        }
        let mut spec = self.workflow(tx, &run).await?.exec_env.clone();
        let repo = tx.setting(&repo_exec_env_key(run.task_id)).await?;
        if let Some(repo) = repo.filter(|v| !v.is_null()) {
            let repo: repo_source::RepoExecEnv =
                serde_json::from_value(repo).map_err(|e| Error::InvalidInput(e.to_string()))?;
            repo.apply(&mut spec);
        }
        Ok(spec)
    }

    /// The run's pinned workflow version, parsed once.
    async fn workflow(&mut self, tx: &mut Tx, run: &Run) -> Result<Arc<Workflow>> {
        let key = (run.workflow_id, run.workflow_version);
        if let Some(workflow) = self.workflows.get(&key) {
            return Ok(workflow.clone());
        }
        let version = tx
            .get_workflow_version(run.workflow_id, run.workflow_version)
            .await?;
        let workflow = sun_workflow::parse(&version.source_yaml).map_err(|errors| {
            let errors: Vec<String> = errors.iter().map(ToString::to_string).collect();
            Error::InvalidInput(format!(
                "workflow {} v{}: {}",
                run.workflow_id,
                run.workflow_version,
                errors.join("; ")
            ))
        })?;
        let workflow = Arc::new(workflow);
        self.workflows.insert(key, workflow.clone());
        Ok(workflow)
    }
}

/// `kernel_setting` key of a task's `exec_env` override (set at creation).
pub(crate) fn task_exec_env_key(task_id: TaskId) -> String {
    format!("task_exec_env.{task_id}")
}

/// `kernel_setting` key of the `.sun/exec_env.yaml` its repo declared at the
/// task's latest run start (`null`: none).
fn repo_exec_env_key(task_id: TaskId) -> String {
    format!("repo_exec_env.{task_id}")
}

/// Puts a `byo_key` runtime's key (the secret of that name in the run's
/// workspace, where the runtime is) into `spec.env[BYO_KEY_ENV]`. A missing
/// secret fails the start (retried, then the step fails) with a message
/// naming it, and so does one the buyer didn't allow a marketplace lease: it
/// never leaves the kernel.
async fn with_byo_key(store: &Store, mut spec: SessionSpec) -> Result<SessionSpec> {
    let Some(name) = spec.byo_key.as_ref().and_then(|b| b.key_secret.clone()) else {
        return Ok(spec);
    };
    let mut read = store.read().await?;
    let task = read.get_run(spec.run_id).await?.task_id;
    let scope = SecretScope::Workspace(read.workspace_of_run(spec.run_id).await?);
    if crate::market::secret_withheld(&mut read, task, &name).await? {
        return Err(Error::Forbidden(format!(
            "the secret `{name}` is withheld from the marketplace runtime; \
             allow it when renting, or use a runtime that needs no key"
        )));
    }
    let value = match read.secret_value(&scope, &name).await {
        Ok(value) => value,
        Err(e) => {
            return Err(match Error::from(e) {
                Error::NotFound(_) => Error::InvalidInput(format!(
                    "the BYO key secret `{name}` is not set in this workspace; set it on \
                     its Secrets page or with `sun secret set {name} --workspace <workspace>`"
                )),
                e => e,
            });
        }
    };
    spec.env.insert(sun_core::BYO_KEY_ENV.into(), value);
    Ok(spec)
}

/// Where a task's sessions and gates work: a worktree of its repo,
/// a task folder when it binds none, or a session dir with one
/// worktree per repo.
/// `InvalidInput` when the task is held or leased on the marketplace and
/// `host` isn't its seller's: it runs only there.
async fn leased(tx: &mut Tx, task_id: TaskId, host: Option<HostId>) -> Result<()> {
    match crate::market::task_lease(tx, task_id).await? {
        Some(lease) if lease.host_id.is_none() || lease.host_id != host => {
            Err(Error::InvalidInput(format!(
                "task {task_id} is leased on the marketplace; it runs only on the seller's host"
            )))
        }
        _ => Ok(()),
    }
}

/// The repos of the task's latest run (`run.repo_ids`, resolved when it
/// was queued) decide: none is a folder, one a worktree on the task's
/// branch, more a session dir of worktrees.
pub(crate) async fn worktree_of(tx: &mut Tx, task_id: TaskId) -> Result<Worktree> {
    let task = tx.get_task(task_id).await?;
    let run = tx.runs_of_task(task_id).await?.pop();
    let mut repos = Vec::new();
    for id in run.map(|r| r.repo_ids).unwrap_or_default() {
        repos.push(tx.get_repo(id).await?);
    }
    if repos.is_empty() {
        return Ok(Worktree::Folder { task_id });
    }
    let branch = tasks::branch_of(tx, &task).await?;
    Ok(match repos.len() {
        1 => {
            let repo = repos.remove(0);
            Worktree::Repo {
                task_id,
                url: repo.url,
                base: Some(repo.default_branch),
                branch,
            }
        }
        _ => Worktree::Repos {
            task_id,
            repos: crate::multi_repo::checkouts(&repos, &branch),
        },
    })
}

/// The latest session of each step attempt (`step.session_id`).
async fn sessions_of(
    tx: &mut Tx,
    steps: &HashMap<StepKey, Step>,
) -> Result<HashMap<StepId, SessionHandle>> {
    let mut out = HashMap::new();
    for step in steps.values() {
        if let Some(id) = step.session_id {
            out.insert(step.id, tx.get_session(id).await?);
        }
    }
    Ok(out)
}

/// Moves a step and journals the move, in the caller's transaction.
async fn move_step(
    tx: &mut Tx,
    step: &mut Step,
    to: StepStatus,
    reason: Option<&str>,
) -> Result<()> {
    let from = tx.set_step_status(step.id, to).await?;
    step.status = to;
    append(
        tx,
        step,
        kind::STEP_STATUS,
        status_payload(step, Some(from), reason),
    )
    .await?;
    // A cancelled step means its run is ending: the run's own move says where
    // the task goes, not the steps left on the way.
    match to {
        StepStatus::Cancelled => Ok(()),
        _ => tasks::follow_run(tx, step.run_id).await,
    }
}

pub(crate) fn status_payload(step: &Step, from: Option<StepStatus>, reason: Option<&str>) -> Value {
    json!({
        "step_id": step.id,
        "attempt": step.attempt,
        "from": from.map(StepStatus::as_str),
        "to": step.status.as_str(),
        "reason": reason,
    })
}

/// Moves a run and journals the move, in the caller's transaction.
async fn move_run(tx: &mut Tx, run_id: RunId, to: RunStatus, reason: Option<&str>) -> Result<()> {
    let from = tx.set_run_status(run_id, to, reason).await?;
    tx.append_event(NewEvent::new(
        run_id,
        None,
        kind::RUN_STATUS,
        json!({ "from": from.as_str(), "to": to.as_str(), "reason": reason }),
    ))
    .await?;
    tasks::follow_run(tx, run_id).await
}

pub(crate) async fn append(tx: &mut Tx, step: &Step, kind: &str, payload: Value) -> Result<()> {
    tx.append_event(NewEvent::new(
        step.run_id,
        Some(step.step_key.clone()),
        kind,
        payload,
    ))
    .await?;
    Ok(())
}

/// Next item of a stream (cancel-safe: nothing is lost if another
/// `select!` branch wins).
pub(crate) async fn next<T>(stream: &mut BoxStream<'static, T>) -> Option<T> {
    std::future::poll_fn(|cx| stream.as_mut().poll_next(cx)).await
}

/// What the `sun` MCP server may ask. Reads use read
/// transactions; `done` goes through the engine task like every transition.
#[async_trait]
impl EngineHandle for Engine {
    /// A `note` row plus its `note` event, in one transaction.
    async fn add_note(
        &self,
        run_id: RunId,
        step_key: StepKey,
        kind: NoteKind,
        body: String,
    ) -> Result<NoteId> {
        let id = NoteId::generate();
        let mut tx = self.store.begin().await?;
        tx.insert_note(&Note {
            id,
            run_id,
            step_key: step_key.clone(),
            kind,
            body: body.clone(),
            superseded_by: None,
        })
        .await?;
        tx.append_event(NewEvent::new(
            run_id,
            Some(step_key),
            kind::NOTE,
            json!({ "note_id": id, "kind": kind, "body": body }),
        ))
        .await?;
        tx.commit().await?;
        Ok(id)
    }

    async fn run_status(&self, run_id: RunId) -> Result<RunStatus> {
        Ok(self.store.read().await?.get_run(run_id).await?.status)
    }

    async fn task(&self, run_id: RunId, step_key: StepKey) -> Result<StepBrief> {
        step_brief(&mut self.store.read().await?, run_id, step_key).await
    }

    async fn get(&self, run_id: RunId, pointer: String) -> Result<String> {
        agent::resolve(&self.store, run_id, &pointer).await
    }

    async fn notes(&self, run_id: RunId) -> Result<Vec<Note>> {
        notes_of(&mut self.store.read().await?, run_id).await
    }

    async fn ask(
        &self,
        run_id: RunId,
        step_key: StepKey,
        question: String,
        options: Vec<String>,
    ) -> Result<String> {
        let answer = self
            .request(|reply| Command::Ask {
                run_id,
                step_key,
                question,
                options,
                reply,
            })
            .await?;
        // Awaited here, off the engine task, with no transaction open.
        answer.await.map_err(|_| {
            Error::Unavailable(format!(
                "run {run_id}: the question was closed (the run ended or Sun restarted)"
            ))
        })
    }

    async fn done(&self, run_id: RunId, step_key: StepKey, summary: String) -> Result<()> {
        self.request(|reply| Command::Done {
            run_id,
            step_key,
            summary,
            reply,
        })
        .await
    }

    /// The task's selected sources on its workspace, hybrid like `retrieve_for_step`.
    async fn search(&self, run_id: RunId, query: String, k: usize) -> Result<String> {
        let task_id = self.store.read().await?.get_run(run_id).await?.task_id;
        let sources = self.task_context(task_id).await?.sources;
        let workspace = self.workspace_of_task(task_id).await?;
        let hits = self
            .search_knowledge(&query, workspace, sources.as_deref(), k)
            .await?;
        Ok(sun_context::render_hits(&query, &hits))
    }

    async fn device_screen(&self, run_id: RunId) -> Result<DeviceScreen> {
        let (host, task_id) = device_home(&self.store, run_id, self.host).await?;
        self.hosts.device_screen(host, task_id).await
    }

    async fn device_input(
        &self,
        run_id: RunId,
        step_key: StepKey,
        input: DeviceInput,
    ) -> Result<()> {
        let (host, task_id) = device_home(&self.store, run_id, self.host).await?;
        let payload = json!({ "action": "input", "input": input });
        self.hosts.device_input(host, task_id, input).await?;
        device_used(&self.store, run_id, step_key, payload).await
    }

    /// In the task's worktree on the run's host, prepared like a verify's.
    async fn device_flow(
        &self,
        run_id: RunId,
        step_key: StepKey,
        mut request: DeviceFlowRequest,
    ) -> Result<DeviceResult> {
        let (host, task_id) = device_home(&self.store, run_id, self.host).await?;
        let worktree = worktree_of(&mut self.store.read().await?, task_id).await?;
        request.cwd = self.hosts.prepare_worktree(host, worktree).await?;
        let result = self.hosts.device_flow(host, request).await?;
        let payload = json!({
            "action": "flow", "device": result.device, "passed": result.passed,
        });
        device_used(&self.store, run_id, step_key, payload).await?;
        Ok(result)
    }
}

/// Event kind of an agent driving its task's device; payload `{action, …}`.
const DEVICE_USED: &str = "device_used";

/// The host a run's device lives on (its worktree's, `run_placed`) and its task.
async fn device_home(store: &Store, run_id: RunId, fallback: HostId) -> Result<(HostId, TaskId)> {
    let mut tx = store.read().await?;
    let task_id = tx.get_run(run_id).await?.task_id;
    let host = queue::placed_host(&mut tx, run_id)
        .await?
        .unwrap_or(fallback);
    Ok((host, task_id))
}

/// Journals an agent's tap or flow on its device (the `EngineHandle` contract).
async fn device_used(
    store: &Store,
    run_id: RunId,
    step_key: StepKey,
    payload: Value,
) -> Result<()> {
    let mut tx = store.begin().await?;
    tx.append_event(NewEvent::new(run_id, Some(step_key), DEVICE_USED, payload))
        .await?;
    Ok(tx.commit().await?)
}

/// The task and step brief (P0, `sun.task`) of a run's step.
async fn step_brief(tx: &mut Tx, run_id: RunId, step_key: StepKey) -> Result<StepBrief> {
    let run = tx.get_run(run_id).await?;
    let task = tx.get_task(run.task_id).await?;
    let version = tx
        .get_workflow_version(run.workflow_id, run.workflow_version)
        .await?;
    let workflow = sun_workflow::parse(&version.source_yaml)
        .map_err(|_| Error::InvalidInput(format!("workflow of run {run_id} no longer parses")))?;
    let def = workflow
        .steps
        .iter()
        .find(|d| d.id == step_key)
        .ok_or_else(|| Error::NotFound(format!("step `{step_key}` of run {run_id}")))?;
    let (goal, context_budget_tokens) = match &def.kind {
        StepKind::Agent {
            prompt,
            context_budget_tokens,
            ..
        } => (prompt.clone(), *context_budget_tokens),
        _ => (String::new(), None),
    };
    // The gates that verify this step are the `gate` steps that need it.
    let gates = workflow
        .steps
        .iter()
        .filter(|d| d.needs.contains(&step_key))
        .filter_map(|d| match &d.kind {
            StepKind::Gate { gates, .. } => Some(gates.iter().map(|g| g.name.clone())),
            _ => None,
        })
        .flatten()
        .collect();
    // Pinned constraints are P0: every pack and envelope carries them.
    let constraints = tx.task_context(task.id).await?.pinned;
    Ok(StepBrief {
        task_id: task.id,
        title: task.title,
        body: task.body,
        step_key,
        goal,
        constraints,
        gates,
        acceptance: Vec::new(),
        context_budget_tokens,
    })
}

/// The run's notes, oldest first.
async fn notes_of(tx: &mut Tx, run_id: RunId) -> Result<Vec<Note>> {
    Ok(tx.notes_of_run(run_id).await?)
}

#[cfg(test)]
mod byo_key_tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use sun_core::TaskStatus;
    use sun_core::model::{Task, Workflow};

    /// A queued run of a fresh task.
    async fn queued(store: &Store) -> (TaskId, RunId) {
        const YAML: &str =
            "name: w\nsteps:\n  - id: ok\n    type: approval\n    who: [role:admin]\n";
        let parsed = sun_workflow::parse(YAML).unwrap();
        let workflow = Workflow {
            id: sun_core::WorkflowId::generate(),
            workspace_id: sun_core::WorkspaceId::MAIN,
            name: parsed.name.clone(),
        };
        let task = Task {
            title: "t".into(),
            workflow_id: Some(workflow.id),
            status: TaskStatus::Backlog,
            ..sun_core::model::Task::new(sun_core::WorkspaceId::MAIN, "")
        };
        let mut tx = store.begin().await.unwrap();
        tx.insert_workflow(&workflow).await.unwrap();
        tx.save_workflow_version(workflow.id, YAML, &parsed.hash)
            .await
            .unwrap();
        tx.insert_task(&task).await.unwrap();
        let run = queue::queue_run(&mut tx, task.id, "created").await.unwrap();
        tx.commit().await.unwrap();
        (task.id, run)
    }

    fn spec(run_id: RunId, key_secret: Option<&str>) -> SessionSpec {
        SessionSpec {
            skills: None,
            session_id: SessionId::generate(),
            run_id,
            step_key: StepKey::new("implement").unwrap(),
            worktree: PathBuf::from("/tmp"),
            envelope: String::new(),
            envelope_parts: None,
            model: None,
            effort: None,
            mcp_config: None,
            env: BTreeMap::new(),
            exec_env: ExecEnvSpec::Bare,
            resume: None,
            permission_mode: None,
            byo_key: Some(sun_core::ByoKey {
                kind: sun_core::ByoKeyKind::OpenaiCompatible,
                endpoint: "https://openrouter.ai/api/v1".into(),
                model: "m".into(),
                key_secret: key_secret.map(Into::into),
            }),
        }
    }

    /// A leased task's session gets only the secrets the buyer allowed.
    #[tokio::test]
    async fn leased_session_env_has_no_unapproved_secret() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).await.unwrap();
        let (task, run) = queued(&store).await;
        let mut tx = store.begin().await.unwrap();
        let scope = SecretScope::Workspace(sun_core::WorkspaceId::MAIN);
        tx.set_secret(&scope, "OR_KEY", "sk-or-0123456789")
            .await
            .unwrap();
        crate::market::hold(&mut tx, task, crate::TaskLease::default())
            .await
            .unwrap();
        tx.commit().await.unwrap();
        let Err(Error::Forbidden(why)) = with_byo_key(&store, spec(run, Some("OR_KEY"))).await
        else {
            panic!("an unapproved secret never reaches the seller");
        };
        assert!(why.contains("`OR_KEY` is withheld"), "{why}");

        let allowed = crate::TaskLease {
            secrets: vec!["OR_KEY".into()],
            ..Default::default()
        };
        let mut tx = store.begin().await.unwrap();
        tx.put_setting(&format!("task_market.{task}"), &json!(allowed))
            .await
            .unwrap();
        tx.commit().await.unwrap();
        let keyed = with_byo_key(&store, spec(run, Some("OR_KEY")))
            .await
            .unwrap();
        assert_eq!(keyed.env[sun_core::BYO_KEY_ENV], "sk-or-0123456789");
        let mut read = store.read().await.unwrap();
        assert!(
            crate::market::secret_withheld(&mut read, task, "GITHUB_TOKEN")
                .await
                .unwrap(),
            "the push token is withheld too unless allowed"
        );
    }

    /// The key is the secret of that name in the run's workspace (Main
    /// here): never the ecosystem's, nor another workspace's.
    #[tokio::test]
    async fn byo_key_start_gets_the_key_from_its_workspace_secret() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).await.unwrap();
        let (_, run) = queued(&store).await;
        let spec = |key_secret| spec(run, key_secret);
        // Keyless (Ollama): nothing to look up.
        let keyless = with_byo_key(&store, spec(None)).await.unwrap();
        assert!(keyless.env.is_empty());

        let Err(Error::InvalidInput(why)) = with_byo_key(&store, spec(Some("OR_KEY"))).await else {
            panic!("a missing secret fails the start");
        };
        assert!(why.contains("`OR_KEY` is not set"), "{why}");

        let mut tx = store.begin().await.unwrap();
        let other = SecretScope::Workspace(sun_core::WorkspaceId::generate());
        for scope in [crate::ecosystem_scope().unwrap(), other] {
            tx.set_secret(&scope, "OR_KEY", "sk-or-not-this-one")
                .await
                .unwrap();
        }
        tx.commit().await.unwrap();
        let Err(Error::InvalidInput(_)) = with_byo_key(&store, spec(Some("OR_KEY"))).await else {
            panic!("another scope's key never reaches this run");
        };
        let mut tx = store.begin().await.unwrap();
        let scope = SecretScope::Workspace(sun_core::WorkspaceId::MAIN);
        tx.set_secret(&scope, "OR_KEY", "sk-or-0123456789")
            .await
            .unwrap();
        tx.commit().await.unwrap();
        let keyed = with_byo_key(&store, spec(Some("OR_KEY"))).await.unwrap();
        assert_eq!(keyed.env[sun_core::BYO_KEY_ENV], "sk-or-0123456789");
    }
}

#[cfg(test)]
mod removal_tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use crate::gates::tests::{LocalHost, mods};

    async fn pending(store: &Store) -> Vec<Removal> {
        pending_removals(&mut store.read().await.unwrap())
            .await
            .unwrap()
    }

    /// A removal planned before a restart is not lost: the next start
    /// removes the worktrees and clears the entry.
    #[tokio::test(flavor = "multi_thread")]
    async fn worktree_removal_survives_a_restart() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).await.unwrap();
        let (host, task_id) = (HostId::generate(), TaskId::generate());
        let mut tx = store.begin().await.unwrap();
        tx.put_setting(WORKTREE_RETENTION, &json!(2)).await.unwrap();
        let kept = plan_removal(&mut tx, host, task_id).await.unwrap();
        assert!(kept.due_at > chrono::Utc::now() + chrono::Duration::minutes(119));
        // Due now (retention 0), and the kernel stops before the timer fires.
        tx.put_setting(WORKTREE_RETENTION, &json!(0)).await.unwrap();
        plan_removal(&mut tx, host, task_id).await.unwrap();
        tx.commit().await.unwrap();
        assert_eq!(pending(&store).await.len(), 1);

        let local = LocalHost::new(dir.path(), |_, _| {});
        let config = crate::EngineConfig {
            allow_bypass: false,
            host,
            adapter: "fake".into(),
            delivery_timeout: Duration::from_secs(5),
            watchdog: crate::WatchdogConfig::default(),
        };
        let (_engine, _task) = Engine::start(store.clone(), local.clone(), mods(json!({})), config)
            .await
            .unwrap();
        for _ in 0..500 {
            if pending(&store).await.is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(*local.removed.lock().unwrap(), [task_id]);
        assert!(pending(&store).await.is_empty());
    }
}

#[cfg(test)]
mod bypass_tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use crate::gates::tests::{LocalHost, mods, seed, until_run_ends};

    /// BLO-139 third path, past the queue: a run started directly (no
    /// placement) of an agent saved in `bypass_permissions` gets no session
    /// while sun.yaml `runtimes.allow_bypass` is off, container or not, and
    /// fails with the reason. With allow on it runs in bypass.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_bypass_session_never_starts_while_the_operator_turned_it_off() {
        let agent = "name: agent
exec_env: { kind: container, image: 'node:22' }
steps:
  - id: implement
    type: agent
    prompt: fix it
    permission_mode: bypass_permissions
";
        for allow_bypass in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let store = Store::open(dir.path()).await.unwrap();
            let local = LocalHost::new(dir.path(), |_, _| {});
            let config = crate::EngineConfig {
                allow_bypass,
                host: HostId::generate(),
                adapter: "fake".into(),
                delivery_timeout: Duration::from_secs(5),
                watchdog: crate::WatchdogConfig::default(),
            };
            let (engine, _task) =
                Engine::start(store.clone(), local.clone(), mods(json!({})), config)
                    .await
                    .unwrap();
            let run_id = seed(&store, agent).await;

            engine.start_run(run_id).await.unwrap();
            let run = until_run_ends(&store, run_id).await;
            let modes = local.modes.lock().unwrap().clone();
            if allow_bypass {
                assert_eq!(run.status, RunStatus::Succeeded, "{:?}", run.end_reason);
                assert_eq!(modes, [Some("bypass_permissions".to_owned())]);
                continue;
            }
            assert_eq!((run.status, modes.len()), (RunStatus::Failed, 0));
            let reason = run.end_reason.unwrap_or_default();
            assert!(reason.contains("runtimes.allow_bypass: false"), "{reason}");
        }
    }
}
