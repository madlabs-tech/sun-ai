//! Queue & flow control.
//!
//! **Pattern:** a single scheduler task (one admitter, so two pumps never
//! start the same run) woken by the event log (**Observer** on
//! [`Store::subscribe`]), never by polling. The database is the truth; the
//! broadcast is only a wake-up, so a lagged receiver just re-reads it.
//!
//! - [`Queue::enqueue`] inserts a `queued` run pinned to the task's latest
//!   workflow version (task `→ queued`, one transaction).
//! - The pump admits queued runs by **priority, then FIFO** (enqueue order =
//!   the `seq` of the run's first event) while the caps per runtime, per host
//!   and per ecosystem allow, and hands each one to [`Engine::start_run`].
//! - **Placement:** each admitted run goes to the least busy host that is
//!   online with the runtime installed and logged in, within the per-host cap
//!   (journaled as `run_placed {host, runtime, memory_mb, cpus}`). Its
//!   worktree and every later session stay there. A host joining wakes the
//!   pump. A run whose exec env (task override, else workflow `exec_env:`)
//!   is a container or VM only goes to hosts that report that kind ready;
//!   a VM's RAM and CPUs count against its host's cap.
//! - Each active run counts against the runtime of its latest session
//!   (`session_assigned`, so failover moves it), else the one it was
//!   placed for.
//! - A `runtime_cooling` event (failover journals one per limit hit) puts
//!   that runtime in `cooling_down` until its end; a run waits only while
//!   every runtime of its workflow's chain is cooling.
//!   Cooldowns are reloaded from the store at start. A timer re-pumps when
//!   the cooldown ends.
//! - While the ecosystem's monthly spend cap is reached (`budget`), runs
//!   with an agent step stay queued; raising the cap wakes the pump, and so
//!   does a timer at the next month's start (UTC), when the count resets.
//! - Task status follows its run inside the engine's own transaction
//!   (`tasks::follow_run`, called from the executor's `move_run` and `move_step`).

use std::cmp::Reverse;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;

use serde_json::json;
use sun_core::model::{ActivityKind, EventRecord, Run};
use sun_core::{
    Error, ExecEnvSpec, HostId, Result, RunId, RunStatus, StateMachine, TaskId, TaskStatus,
    Timestamp,
};
use sun_store::{
    EventLog, HostRepo, NewEvent, RunRepo, RuntimeRepo, SettingRepo, Store, TaskRepo, Tx,
    WorkflowRepo,
};
use tokio::sync::Notify;
use tokio::sync::broadcast::Receiver;
use tokio::sync::broadcast::error::{RecvError, TryRecvError};
use tokio::task::JoinHandle;

use crate::Engine;
use crate::executor::{Ready, failover, task_exec_env_key, tasks};

/// Concurrency caps. A missing entry means no cap.
#[derive(Debug, Clone, Default)]
pub struct Caps {
    /// Active runs per runtime (a workflow chain entry, e.g. `claude-code`).
    pub per_runtime: BTreeMap<String, usize>,
    /// Per host, by host name; `*` applies to hosts not named.
    pub per_host: BTreeMap<String, HostCap>,
    /// Active runs in the ecosystem (one per kernel).
    pub per_ecosystem: Option<usize>,
}

impl Caps {
    /// The cap of the host named `name`.
    fn host(&self, name: &str) -> HostCap {
        let cap = self.per_host.get(name).or_else(|| self.per_host.get("*"));
        cap.cloned().unwrap_or_default()
    }
}

/// What one host may carry at once. `None` = no cap.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HostCap {
    /// Active runs.
    #[serde(default)]
    pub runs: Option<usize>,
    /// MiB of RAM given to the VMs of active runs.
    #[serde(default)]
    pub memory_mb: Option<u64>,
    /// Virtual CPUs given to the VMs of active runs.
    #[serde(default)]
    pub cpus: Option<u32>,
}

/// What an active run takes from its host besides a slot: its VM.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct Load {
    memory_mb: u64,
    cpus: u32,
}

/// Lima's template defaults, counted for a VM that doesn't set its own.
const VM_DEFAULT: Load = Load {
    memory_mb: 4096,
    cpus: 4,
};

impl Load {
    fn of(env: &ExecEnvSpec) -> Self {
        match env {
            ExecEnvSpec::Vm(vm) => Self {
                memory_mb: vm.memory_mb.unwrap_or(VM_DEFAULT.memory_mb),
                cpus: vm.cpus.unwrap_or(VM_DEFAULT.cpus),
            },
            _ => Self::default(),
        }
    }

    fn add(&mut self, other: Self) {
        self.memory_mb += other.memory_mb;
        self.cpus += other.cpus;
    }
}

/// Where and as what a run was started: the `run_placed` payload.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct Placement {
    pub host: HostId,
    /// The runtime the queue admitted it for.
    #[serde(default)]
    pub runtime: Option<String>,
    /// Its VM's RAM and CPUs (0 outside a VM).
    #[serde(default)]
    pub memory_mb: u64,
    #[serde(default)]
    pub cpus: u32,
}

impl Placement {
    /// Placed on `host`, nothing else known.
    pub(crate) fn on(host: HostId) -> Self {
        Self {
            host,
            runtime: None,
            memory_mb: 0,
            cpus: 0,
        }
    }

    fn load(&self) -> Load {
        Load {
            memory_mb: self.memory_mb,
            cpus: self.cpus,
        }
    }
}

/// Where a run executes: the unit the caps and cooldowns count.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Slot {
    /// `RuntimeAdapter::id`.
    pub runtime: String,
    /// The host.
    pub host: HostId,
}

/// How the queue admits work.
#[derive(Debug, Clone)]
pub struct QueueConfig {
    /// The runtime runs are admitted for (`EngineConfig::adapter`), and the
    /// host used while no host reports that runtime at all
    /// (`EngineConfig::host`: single-host setups and tests).
    pub slot: Slot,
    /// Concurrency caps.
    pub caps: Caps,
}

/// Handle to the scheduler task. Cheap to clone.
#[derive(Debug, Clone)]
pub struct Queue {
    store: Store,
    wake: Arc<Notify>,
}

impl Queue {
    /// Spawns the scheduler. It admits whatever is already queued, then runs
    /// until the engine stops (`Err`) or it is aborted.
    pub fn start(
        store: Store,
        engine: Engine,
        config: QueueConfig,
    ) -> (Self, JoinHandle<Result<()>>) {
        Self::start_with_clock(store, engine, config, chrono::Utc::now)
    }

    /// [`Queue::start`] reading the time from `clock`: the spend cap's month
    /// and when its period rolls over, cooldown ends. For tests.
    pub fn start_with_clock(
        store: Store,
        engine: Engine,
        config: QueueConfig,
        clock: fn() -> Timestamp,
    ) -> (Self, JoinHandle<Result<()>>) {
        let wake = Arc::new(Notify::new());
        let scheduler = Scheduler {
            store: store.clone(),
            engine,
            config,
            clock,
            cooling: HashMap::new(),
            held_until: None,
            sessions: HashMap::new(),
        };
        let task = tokio::spawn(scheduler.run(wake.clone()));
        (Self { store, wake }, task)
    }

    /// Queues a run of `task` on its workflow's latest version, with that
    /// version's budget (a retry: the run's trigger is `retry`). The task
    /// moves to `todo`; a `done` task can't.
    pub async fn enqueue(&self, task_id: TaskId) -> Result<RunId> {
        let mut tx = self.store.begin().await?;
        let run_id = queue_run(&mut tx, task_id, "retry").await?;
        tx.commit().await?;
        self.wake.notify_one();
        Ok(run_id)
    }

    /// Re-checks the queue now, e.g. because a host connected.
    pub fn wake(&self) {
        self.wake.notify_one();
    }

    /// Queued runs in admission order; a run's index is its queue position
    /// on the workspace.
    pub async fn waiting(&self) -> Result<Vec<Run>> {
        let (mut waiting, _) = load(&self.store, HostId::generate()).await?;
        sort(&mut waiting);
        Ok(waiting.into_iter().map(|w| w.run).collect())
    }
}

/// [`Queue::enqueue`] in the caller's transaction, so a task can be created
/// and queued atomically. The committed `run_status` event wakes the pump.
/// `trigger` (`created`, `trigger`, `retry`, `moved`) is the reason of the
/// run's first `run_status` event: why it runs.
pub(crate) async fn queue_run(tx: &mut Tx, task_id: TaskId, trigger: &str) -> Result<RunId> {
    let task = tx.get_task(task_id).await?;
    let workflow_id = task.workflow_id.ok_or_else(|| {
        Error::InvalidInput(format!("task {task_id} has no workflow, so it never runs"))
    })?;
    let busy = tx.runs_of_task(task_id).await?.pop();
    if let Some(run) = busy.filter(|r| !r.status.is_terminal()) {
        return Err(Error::InvalidInput(format!(
            "task {task_id} already has run {} going",
            run.id
        )));
    }
    let version = tx
        .latest_workflow_version(workflow_id)
        .await?
        .ok_or_else(|| Error::NotFound(format!("workflow {workflow_id} has no version")))?;
    let workflow = sun_workflow::parse(&version.source_yaml).map_err(|errors| {
        let errors: Vec<String> = errors.iter().map(ToString::to_string).collect();
        Error::InvalidInput(format!(
            "workflow {} v{}: {}",
            version.workflow_id,
            version.version,
            errors.join("; ")
        ))
    })?;
    let repo_ids = crate::repo_resolve::run_repos(tx, &task, workflow_id, &workflow).await?;
    let run = Run {
        id: RunId::generate(),
        task_id,
        workflow_id: version.workflow_id,
        workflow_version: version.version,
        status: RunStatus::Queued,
        end_reason: None,
        budget: workflow.budget,
        repo_ids,
    };
    tx.insert_run(&run).await?;
    tx.append_event(NewEvent::new(
        run.id,
        None,
        "run_status",
        json!({ "from": null, "to": RunStatus::Queued.as_str(), "reason": trigger }),
    ))
    .await?;
    let queued = json!({ "run_id": run.id, "trigger": trigger });
    tasks::log(tx, task_id, ActivityKind::Run, None, queued).await?;
    tasks::set_status(tx, task_id, TaskStatus::Todo, None, Some(run.id)).await?;
    Ok(run.id)
}

/// A queued run with its ordering keys.
struct Waiting {
    run: Run,
    /// [`sun_core::model::Priority::rank`] of its task.
    priority: u8,
    /// `seq` of the run's first event: enqueue order.
    seq: i64,
}

fn sort(waiting: &mut [Waiting]) {
    waiting.sort_by_key(|w| (Reverse(w.priority), w.seq));
}

/// Event kind of a run's placement; payload `{host}`.
pub(crate) const RUN_PLACED: &str = "run_placed";

/// Where `run_id` was placed, if the queue placed it. `run_placed` is
/// written with the run's start, among its first few events.
/// A run that moved hosts mid-step ([`moved_key`]) is placed on the new one.
pub(crate) async fn placement(tx: &mut Tx, run_id: RunId) -> Result<Option<Placement>> {
    let first = tx.events_after(Some(run_id), 0, 16).await?;
    let placed: Option<Placement> = first
        .iter()
        .find(|e| e.kind == RUN_PLACED)
        .and_then(|e| serde_json::from_value(e.payload.clone()).ok());
    let moved = tx.setting(&moved_key(run_id)).await?;
    Ok(
        match moved.and_then(|v| serde_json::from_value::<HostId>(v).ok()) {
            Some(host) => Some(Placement {
                host,
                ..placed.unwrap_or_else(|| Placement::on(host))
            }),
            None => placed,
        },
    )
}

/// `kernel_setting` key of the host a running run moved to mid-step (a
/// marketplace failover's hand-off), written with the move.
pub(crate) fn moved_key(run_id: RunId) -> String {
    format!("run_moved.{run_id}")
}

/// The host `run_id` was placed on, if the queue placed it.
pub(crate) async fn placed_host(tx: &mut Tx, run_id: RunId) -> Result<Option<HostId>> {
    Ok(placement(tx, run_id).await?.map(|p| p.host))
}

/// An active run (started, not ended) and where it was placed (`fallback`
/// when it was started unplaced).
type Active = (RunId, Placement);

/// Queued runs, and every active run.
// ponytail: N+1 reads per pump (task + first events per run); add an
// ordered `queued_runs` query and a run host column when queues get long.
async fn load(store: &Store, fallback: HostId) -> Result<(Vec<Waiting>, Vec<Active>)> {
    let mut tx = store.read().await?;
    let mut waiting = Vec::new();
    let mut active = Vec::new();
    for run in tx.active_runs().await? {
        if run.status != RunStatus::Queued {
            let placed = placement(&mut tx, run.id).await?;
            active.push((run.id, placed.unwrap_or_else(|| Placement::on(fallback))));
            continue;
        }
        let priority = tx.get_task(run.task_id).await?.priority.rank();
        let seq = tx
            .events_after(Some(run.id), 0, 1)
            .await?
            .first()
            .map_or(i64::MAX, |e| e.seq);
        waiting.push(Waiting { run, priority, seq });
    }
    Ok((waiting, active))
}

/// A queued run as the admitter sees it.
#[derive(Debug, Clone)]
struct Want {
    run_id: RunId,
    /// The runtime it would start on.
    runtime: String,
    /// The exec env kind it needs (`bare`, `container`, `vm`).
    env: String,
    load: Load,
}

/// A host a run may go to: its ready exec env kinds and its cap.
type Candidate = (HostId, Vec<String>, HostCap);

/// Which of the (sorted) `waiting` runs to start now, and where: the least
/// busy of `hosts` (in preference order) that can run the run's exec env and
/// has room for it (a run slot, and its VM's RAM and CPUs) and whose CLI for
/// the run's runtime is ready (`runs_on`). `active` holds the slot and load
/// of every active run. Runs are skipped, not blocked on: a full or cooling
/// slot doesn't hold back other work.
fn admit(
    waiting: &[Want],
    hosts: &[Candidate],
    active: &[(Slot, Load)],
    caps: &Caps,
    cooling: impl Fn(&str) -> bool,
    runs_on: impl Fn(&HostId, &str) -> bool,
) -> Vec<(RunId, HostId)> {
    let mut runtimes: HashMap<&str, usize> = HashMap::new();
    let mut per_host: HashMap<HostId, (usize, Load)> = HashMap::new();
    for (slot, load) in active {
        *runtimes.entry(&slot.runtime).or_default() += 1;
        let used = per_host.entry(slot.host).or_default();
        used.0 += 1;
        used.1.add(*load);
    }
    let mut total = active.len();
    let mut out = Vec::new();
    let full = |cap: Option<&usize>, used: usize| cap.is_some_and(|cap| used >= *cap);
    for want in waiting {
        let runtime = want.runtime.as_str();
        let runtime_used = runtimes.get(runtime).copied().unwrap_or(0);
        if cooling(runtime)
            || full(caps.per_ecosystem.as_ref(), total)
            || full(caps.per_runtime.get(runtime), runtime_used)
        {
            continue;
        }
        let used = |host: &HostId| per_host.get(host).copied().unwrap_or_default();
        let fits = |(host, envs, cap): &&Candidate| {
            let (runs, load) = used(host);
            envs.contains(&want.env)
                && runs_on(host, runtime)
                && !full(cap.runs.as_ref(), runs)
                && cap
                    .memory_mb
                    .is_none_or(|max| load.memory_mb + want.load.memory_mb <= max)
                && cap.cpus.is_none_or(|max| load.cpus + want.load.cpus <= max)
        };
        let Some(host) = hosts
            .iter()
            .filter(fits)
            .map(|(h, ..)| *h)
            .min_by_key(|h| used(h).0)
        else {
            continue;
        };
        *runtimes.entry(runtime).or_default() += 1;
        let used = per_host.entry(host).or_default();
        used.0 += 1;
        used.1.add(want.load);
        total += 1;
        out.push((want.run_id, host));
    }
    out
}

struct Scheduler {
    store: Store,
    engine: Engine,
    config: QueueConfig,
    /// The time ([`Queue::start_with_clock`]).
    clock: fn() -> Timestamp,
    /// `cooling_down` runtimes → end of the cooldown (a cache of
    /// `runtime_state`, which failover writes).
    cooling: HashMap<String, Timestamp>,
    /// The spend cap holds runs until this month start.
    held_until: Option<Timestamp>,
    /// Active runs → the runtime of their latest session (`session_assigned`).
    // ponytail: learnt from live events; after a restart a run counts as its
    // placement's runtime until its next session.
    sessions: HashMap<RunId, String>,
}

impl Scheduler {
    async fn run(mut self, wake: Arc<Notify>) -> Result<()> {
        let mut events = self.store.subscribe();
        self.reload_cooldowns().await?;
        let mut dirty = true;
        loop {
            if dirty {
                dirty = self.pump(&mut events).await?;
                if dirty {
                    continue;
                }
            }
            // ponytail: tokio's timer runs on monotonic time, so a machine
            // asleep at the deadline pumps that much late (or on the next event).
            let next = self.cooling.values().chain(&self.held_until).min();
            let until = next.map(|end| (*end - (self.clock)()).to_std().unwrap_or_default());
            dirty = tokio::select! {
                () = wake.notified() => true,
                event = events.recv() => match event {
                    Ok(event) => self.observe(&event),
                    Err(RecvError::Lagged(_)) => {
                        self.reload_cooldowns().await?;
                        true
                    }
                    Err(RecvError::Closed) => return Ok(()),
                },
                () = tokio::time::sleep(until.unwrap_or_default()), if until.is_some() => true,
            };
        }
    }

    /// Merges the cooldowns stored in `runtime_state` into the cache: at start,
    /// and when the event receiver lagged and may have dropped a
    /// `runtime_cooling` (failover writes both in one transaction).
    async fn reload_cooldowns(&mut self) -> Result<()> {
        for (runtime, until) in self.store.read().await?.cooldowns().await? {
            self.cool(runtime, until);
        }
        Ok(())
    }

    /// `runtime` cools down until `until`, or longer if it already was.
    fn cool(&mut self, runtime: String, until: Timestamp) {
        let end = self.cooling.entry(runtime).or_insert(until);
        *end = (*end).max(until);
    }

    /// Reacts to one committed event; true when the queue should re-pump.
    fn observe(&mut self, event: &EventRecord) -> bool {
        match event.kind.as_str() {
            "run_status" => true,
            "runtime_cooling" => {
                let Ok(cooling) = serde_json::from_value::<Cooling>(event.payload.clone()) else {
                    return false;
                };
                self.cool(cooling.runtime, cooling.until);
                true
            }
            "session_assigned" => {
                if let Some(runtime) = event.payload["adapter"].as_str() {
                    self.sessions.insert(event.run_id, runtime.to_owned());
                }
                false
            }
            _ => false,
        }
    }

    /// One scheduling pass; true when an event it caught up on wants another.
    async fn pump(&mut self, events: &mut Receiver<EventRecord>) -> Result<bool> {
        let (mut waiting, active) = load(&self.store, self.config.slot.host).await?;
        // Catch up on events after reading the queue, so a run seen there is
        // placed knowing every cooldown committed before it (a commit
        // broadcasts before it returns). Waiting for `select!` can't promise
        // that: it picks a ready branch at random, and a pass may already be
        // running when the cooldown commits.
        let mut again = false;
        loop {
            match events.try_recv() {
                Ok(event) => again |= self.observe(&event),
                Err(TryRecvError::Lagged(_)) => {
                    self.reload_cooldowns().await?;
                    again = true;
                }
                Err(TryRecvError::Empty | TryRecvError::Closed) => break,
            }
        }
        let now = (self.clock)();
        self.cooling.retain(|_, end| *end > now);
        let slot = &self.config.slot;
        sort(&mut waiting);
        // Each run counts against the first runtime of its chain that is not
        // cooling (failover picks the same one at dispatch), and goes to a
        // host whose CLI for that runtime is ready.
        let mut tx = self.store.read().await?;
        let mut runs = Vec::with_capacity(waiting.len());
        // Leased runs go only to the seller's host that joined for them.
        let mut leased = Vec::new();
        // Runs whose chain reached a `market:` entry.
        let mut to_market = Vec::new();
        let capped = crate::budget::ecosystem_over(&mut tx, now).await?.is_some();
        self.held_until = capped.then(|| crate::budget::next_month_start(now));
        for w in &waiting {
            if let Some(lease) = crate::market::task_lease(&mut tx, w.run.task_id).await? {
                leased.push((w.run.id, lease.host_id));
                continue;
            }
            let version = tx
                .get_workflow_version(w.run.workflow_id, w.run.workflow_version)
                .await?;
            // An unparsable workflow fails at start; admit it as the default.
            let workflow = sun_workflow::parse(&version.source_yaml).ok();
            let spends = workflow.as_ref().is_none_or(|w| {
                w.steps
                    .iter()
                    .any(|s| matches!(s.kind, sun_workflow::StepKind::Agent { .. }))
            });
            if capped && spends {
                continue;
            }
            let chain = workflow
                .as_ref()
                .map(|workflow| failover::chain(workflow, &slot.runtime))
                .unwrap_or_else(|| vec![slot.runtime.clone()]);
            let runtime = chain
                .iter()
                .find(|r| !self.cooling.contains_key(*r))
                .unwrap_or(&chain[0]);
            if let Some(adapter) = runtime.strip_prefix(crate::market::MARKET_PREFIX) {
                to_market.push((w.run.id, adapter.to_owned()));
                continue;
            }
            let own = tx.setting(&task_exec_env_key(w.run.task_id)).await?;
            let env = match own.filter(|v| !v.is_null()) {
                Some(spec) => serde_json::from_value::<ExecEnvSpec>(spec).unwrap_or_default(),
                None => workflow
                    .as_ref()
                    .map(|w| w.exec_env.clone())
                    .unwrap_or_default(),
            };
            runs.push(Want {
                run_id: w.run.id,
                runtime: runtime.clone(),
                env: env.kind().to_owned(),
                load: Load::of(&env),
            });
        }
        let waiting = runs;
        self.sessions
            .retain(|run, _| active.iter().any(|(id, _)| id == run));
        let active: Vec<(Slot, Load)> = active
            .into_iter()
            .map(|(run_id, placed)| {
                let runtime = self.sessions.get(&run_id).cloned();
                let runtime = runtime.or_else(|| placed.runtime.clone());
                let slot = Slot {
                    runtime: runtime.unwrap_or_else(|| slot.runtime.clone()),
                    host: placed.host,
                };
                (slot, placed.load())
            })
            .collect();
        let mut runtimes: Vec<&str> = waiting.iter().map(|w| w.runtime.as_str()).collect();
        runtimes.push(&slot.runtime);
        runtimes.sort_unstable();
        runtimes.dedup();
        // Each host once, with its ready exec env kinds; and which runtimes
        // each one is ready for.
        let mut ready: Vec<(HostId, Vec<String>)> = Vec::new();
        let mut runs_on: HashSet<(HostId, String)> = HashSet::new();
        for runtime in runtimes {
            let hosts = match self.engine.ready_hosts(runtime).await? {
                Ready::Hosts(hosts) => hosts,
                // Unknown capabilities: try anything, `prepare` says why not.
                Ready::Unreported => vec![(
                    slot.host,
                    ["bare", "container", "vm"].map(String::from).to_vec(),
                )],
            };
            for (id, envs) in hosts {
                runs_on.insert((id, runtime.to_owned()));
                if !ready.iter().any(|(h, _)| *h == id) {
                    ready.push((id, envs));
                }
            }
        }
        let names: HashMap<HostId, String> = tx
            .hosts()
            .await?
            .into_iter()
            .map(|h| (h.id, h.name))
            .collect();
        let mut hosts: Vec<Candidate> = Vec::with_capacity(ready.len());
        for (id, envs) in &ready {
            if !crate::market::is_market_host(&mut tx, *id).await? {
                let name = names.get(id).map_or("", String::as_str);
                hosts.push((*id, envs.clone(), self.config.caps.host(name)));
            }
        }
        drop(tx);
        for (run_id, adapter) in to_market {
            let mut tx = self.store.begin().await?;
            if crate::market::fail_over(&mut tx, run_id, &adapter).await? {
                tx.commit().await?;
            }
        }
        let wants: HashMap<RunId, &Want> = waiting.iter().map(|w| (w.run_id, w)).collect();
        let mut start: Vec<(RunId, Placement)> = admit(
            &waiting,
            &hosts,
            &active,
            &self.config.caps,
            |runtime| self.cooling.contains_key(runtime),
            |host, runtime| runs_on.contains(&(*host, runtime.to_owned())),
        )
        .into_iter()
        .map(|(run_id, host)| {
            let want = wants.get(&run_id);
            let placement = Placement {
                runtime: want.map(|w| w.runtime.clone()),
                memory_mb: want.map_or(0, |w| w.load.memory_mb),
                cpus: want.map_or(0, |w| w.load.cpus),
                ..Placement::on(host)
            };
            (run_id, placement)
        })
        .collect();
        // The seller's host has one paid slot for its task (run in a vm);
        // only the ecosystem's cap (the plan's agents at once) still applies.
        for (run_id, host) in leased {
            let used = active.len() + start.len();
            let room = self.config.caps.per_ecosystem.is_none_or(|cap| used < cap);
            let fits = |h: &HostId| {
                ready
                    .iter()
                    .any(|(id, envs)| id == h && envs.iter().any(|e| e == "vm"))
            };
            if let Some(host) = host.filter(|h| room && fits(h)) {
                start.push((run_id, Placement::on(host)));
            }
        }
        for (run_id, placement) in start {
            // Other errors (e.g. cancelled since it was read) leave the run
            // out of the queue anyway; only a stopped engine stops the queue.
            if let Err(e @ Error::Unavailable(_)) =
                self.engine.start_placed(run_id, placement).await
            {
                return Err(e);
            }
        }
        Ok(again)
    }
}

/// A `runtime_cooling` event: `{runtime, until}`.
#[derive(serde::Deserialize)]
struct Cooling {
    runtime: String,
    until: Timestamp,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn slot(runtime: &str, host: HostId) -> (Slot, Load) {
        let slot = Slot {
            runtime: runtime.into(),
            host,
        };
        (slot, Load::default())
    }

    fn want(run_id: RunId, runtime: &str, env: &str) -> Want {
        Want {
            run_id,
            runtime: runtime.into(),
            env: env.into(),
            load: Load::default(),
        }
    }

    /// Every host is ready for every runtime.
    fn any(_: &HostId, _: &str) -> bool {
        true
    }

    fn runs_cap(cap: usize) -> HostCap {
        HostCap {
            runs: Some(cap),
            ..HostCap::default()
        }
    }

    fn waiting(priority: u8, seq: i64) -> Waiting {
        Waiting {
            run: Run {
                id: RunId::generate(),
                task_id: TaskId::generate(),
                workflow_id: sun_core::WorkflowId::generate(),
                workflow_version: 1,
                status: RunStatus::Queued,
                end_reason: None,
                budget: Default::default(),
                repo_ids: Vec::new(),
            },
            priority,
            seq,
        }
    }

    #[test]
    fn queue_orders_by_priority_then_fifo() {
        let mut list = vec![waiting(0, 1), waiting(5, 3), waiting(0, 2), waiting(5, 4)];
        let ids: Vec<RunId> = list.iter().map(|w| w.run.id).collect();
        sort(&mut list);
        let order: Vec<RunId> = list.iter().map(|w| w.run.id).collect();
        assert_eq!(order, [ids[1], ids[3], ids[0], ids[2]]);
    }

    #[test]
    fn queue_admits_within_every_cap_and_skips_full_or_cooling_slots() {
        let (h1, h2) = (HostId::generate(), HostId::generate());
        let runs: Vec<RunId> = (0..6).map(|_| RunId::generate()).collect();
        let waiting = vec![
            want(runs[0], "claude", "bare"),
            want(runs[1], "claude", "bare"), // runtime cap 2: one active + runs[0]
            want(runs[2], "codex", "bare"),
            want(runs[3], "codex", "bare"),
            want(runs[4], "opencode", "bare"), // cooling
            want(runs[5], "codex", "bare"),    // ecosystem cap 5: full
        ];
        let bare = |h: HostId, cap: HostCap| (h, vec!["bare".to_owned()], cap);
        let caps = Caps {
            per_runtime: BTreeMap::from([("claude".into(), 2)]),
            per_ecosystem: Some(5),
            ..Caps::default()
        };
        let active = [slot("claude", h1), slot("codex", h2)];
        let hosts = [bare(h1, runs_cap(2)), bare(h2, HostCap::default())];
        let start = admit(&waiting, &hosts, &active, &caps, |r| r == "opencode", any);
        // h1 (1 active) and h2 (1 active) tie: h1 first; then h2 is less busy;
        // then h1 is full (cap 2) so h2 takes the rest.
        assert_eq!(start, [(runs[0], h1), (runs[2], h2), (runs[3], h2)]);

        let one = [bare(h2, HostCap::default())];
        let unlimited = admit(&waiting, &one, &active, &Caps::default(), |_| false, any);
        assert_eq!(unlimited.len(), runs.len());
        assert!(unlimited.iter().all(|(_, host)| *host == h2));
        assert!(admit(&waiting, &[], &active, &Caps::default(), |_| false, any).is_empty());
    }

    /// A run goes to a host whose CLI for its own runtime is ready, not
    /// the default runtime's.
    #[test]
    fn queue_places_runs_on_hosts_ready_for_their_runtime() {
        let (h1, h2) = (HostId::generate(), HostId::generate());
        let runs: Vec<RunId> = (0..3).map(|_| RunId::generate()).collect();
        let waiting = vec![
            want(runs[0], "codex", "bare"),
            want(runs[1], "claude", "bare"),
            want(runs[2], "opencode", "bare"), // ready nowhere: waits
        ];
        let bare = |h: HostId| (h, vec!["bare".to_owned()], HostCap::default());
        let hosts = [bare(h1), bare(h2)];
        // h1 has only claude ready, h2 only codex.
        let runs_on =
            |h: &HostId, r: &str| (*h == h1 && r == "claude") || (*h == h2 && r == "codex");
        let start = admit(&waiting, &hosts, &[], &Caps::default(), |_| false, runs_on);
        assert_eq!(start, [(runs[0], h2), (runs[1], h1)]);
    }

    #[test]
    fn queue_counts_vm_ram_and_cpus_against_host_caps() {
        let host = HostId::generate();
        let runs: Vec<RunId> = (0..3).map(|_| RunId::generate()).collect();
        let vm = |run_id, memory_mb, cpus| Want {
            load: Load { memory_mb, cpus },
            ..want(run_id, "claude", "vm")
        };
        // One 4 GiB / 2 CPU VM already runs; the host gives VMs 8 GiB / 4 CPUs.
        let active = [(
            Slot {
                runtime: "claude".into(),
                host,
            },
            Load {
                memory_mb: 4096,
                cpus: 2,
            },
        )];
        let cap = HostCap {
            memory_mb: Some(8192),
            cpus: Some(4),
            ..HostCap::default()
        };
        let hosts = [(host, vec!["bare".to_owned(), "vm".to_owned()], cap)];
        let waiting = [
            vm(runs[0], 6144, 1),            // RAM: 4 + 6 > 8
            vm(runs[1], 2048, 2),            // fits exactly
            want(runs[2], "claude", "bare"), // no VM: only the run slot counts
        ];
        let start = admit(&waiting, &hosts, &active, &Caps::default(), |_| false, any);
        assert_eq!(start, [(runs[1], host), (runs[2], host)]);
        // An unset VM size counts Lima's defaults.
        let spec = ExecEnvSpec::Vm(sun_core::VmSpec {
            template: "default".into(),
            memory_mb: None,
            cpus: Some(1),
            network: None,
            keep: false,
        });
        assert_eq!(
            Load::of(&spec),
            Load {
                memory_mb: 4096,
                cpus: 1
            }
        );
        // `*` caps hosts that aren't named.
        let caps = Caps {
            per_host: BTreeMap::from([("big".into(), runs_cap(9)), ("*".into(), runs_cap(1))]),
            ..Caps::default()
        };
        assert_eq!(caps.host("big").runs, Some(9));
        assert_eq!(caps.host("other").runs, Some(1));
    }

    #[test]
    fn queue_places_container_and_vm_runs_only_on_hosts_with_that_env_ready() {
        let (bare_host, docker_host) = (HostId::generate(), HostId::generate());
        let runs: Vec<RunId> = (0..3).map(|_| RunId::generate()).collect();
        let waiting = vec![
            want(runs[0], "claude", "container"),
            want(runs[1], "claude", "vm"), // no host has a VM ready: waits
            want(runs[2], "claude", "bare"),
        ];
        let hosts = [
            (bare_host, vec!["bare".to_owned()], HostCap::default()),
            (
                docker_host,
                vec!["bare".to_owned(), "container".to_owned()],
                HostCap::default(),
            ),
        ];
        let start = admit(&waiting, &hosts, &[], &Caps::default(), |_| false, any);
        assert_eq!(start, [(runs[0], docker_host), (runs[2], bare_host)]);
    }

    /// A queued run gets the workspace repos the task and workflow text
    /// name; a coding workflow naming none fails fast while the workspace
    /// has repos, and runs in a folder when it has none.
    #[tokio::test]
    async fn queue_run_resolves_repos_from_the_task_and_workflow_text() {
        use sun_core::model::{Task, Workflow};
        use sun_store::RepoRepo;

        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).await.unwrap();
        let yaml = "name: ship\ndescription: Deploys VPS Infra\nsteps:\n  \
                    - {id: pr, type: action, use: github.open_pr}\n";
        let parsed = sun_workflow::parse(yaml).unwrap();
        let workflow = Workflow {
            id: sun_core::WorkflowId::generate(),
            workspace_id: sun_core::WorkspaceId::MAIN,
            name: parsed.name.clone(),
        };
        let task = |title: &str| Task {
            workflow_id: Some(workflow.id),
            status: TaskStatus::Backlog,
            ..Task::new(sun_core::WorkspaceId::MAIN, title)
        };
        let (unnamed, named) = (task("Rotate keys"), task("Fix the WEB header"));
        let mut tx = store.begin().await.unwrap();
        tx.insert_workflow(&workflow).await.unwrap();
        tx.save_workflow_version(workflow.id, yaml, &parsed.hash)
            .await
            .unwrap();
        tx.insert_task(&unnamed).await.unwrap();
        tx.insert_task(&named).await.unwrap();
        // No repos at all: a coding workflow still runs, in a folder.
        let run = queue_run(&mut tx, unnamed.id, "created").await.unwrap();
        assert!(tx.get_run(run).await.unwrap().repo_ids.is_empty());
        tx.commit().await.unwrap();

        let web = crate::gates::tests::repo("https://github.com/acme/web.git");
        let infra = crate::gates::tests::repo("git@github.com:hryer/vps-infra.git");
        let mut tx = store.begin().await.unwrap();
        tx.insert_repo(&web).await.unwrap();
        tx.insert_repo(&infra).await.unwrap();
        // The workflow's description names infra; the title names web too:
        // both, in the workspace's list order.
        let run = queue_run(&mut tx, named.id, "created").await.unwrap();
        let listed: Vec<sun_core::RepoId> = tx
            .repos(sun_core::WorkspaceId::MAIN)
            .await
            .unwrap()
            .iter()
            .map(|r| r.id)
            .collect();
        assert_eq!(listed.len(), 2);
        assert_eq!(tx.get_run(run).await.unwrap().repo_ids, listed);
        tx.commit().await.unwrap();

        // Workflows naming no repo themselves: a coding one (an open_pr
        // action) fails fast, a writing one runs in a folder.
        let release = "name: release\nsteps:\n  - {id: pr, type: action, use: github.open_pr}\n";
        let plain = "name: prd\nsteps:\n  - {id: a, type: agent, prompt: x}\n";
        let mut tx = store.begin().await.unwrap();
        let mut save = async |yaml: &str| {
            let parsed = sun_workflow::parse(yaml).unwrap();
            let workflow = Workflow {
                id: sun_core::WorkflowId::generate(),
                workspace_id: sun_core::WorkspaceId::MAIN,
                name: parsed.name.clone(),
            };
            tx.insert_workflow(&workflow).await.unwrap();
            tx.save_workflow_version(workflow.id, yaml, &parsed.hash)
                .await
                .unwrap();
            workflow.id
        };
        // A writing workflow that says it needs a repo is checked like a
        // coding one.
        let needs = "name: review\nneeds_repo: true\nsteps:\n  - {id: a, type: agent, prompt: x}\n";
        let (release, prd, review) = (save(release).await, save(plain).await, save(needs).await);
        let coding = Task {
            workflow_id: Some(release),
            status: TaskStatus::Backlog,
            ..Task::new(sun_core::WorkspaceId::MAIN, "Rotate keys")
        };
        let writing = Task {
            workflow_id: Some(prd),
            ..Task::new(sun_core::WorkspaceId::MAIN, "Rotate keys")
        };
        tx.insert_task(&coding).await.unwrap();
        tx.insert_task(&writing).await.unwrap();
        let err = queue_run(&mut tx, coding.id, "created").await.unwrap_err();
        assert!(
            err.to_string()
                .contains("No repository mentioned; add one to the task or workflow"),
            "{err}"
        );
        let run = queue_run(&mut tx, writing.id, "created").await.unwrap();
        assert!(tx.get_run(run).await.unwrap().repo_ids.is_empty());
        let forced = Task {
            workflow_id: Some(review),
            status: TaskStatus::Backlog,
            ..Task::new(sun_core::WorkspaceId::MAIN, "Rotate keys")
        };
        tx.insert_task(&forced).await.unwrap();
        let err = queue_run(&mut tx, forced.id, "created").await.unwrap_err();
        assert!(
            err.to_string().contains("No repository mentioned"),
            "needs_repo: {err}"
        );
        let named = Task {
            workflow_id: Some(review),
            status: TaskStatus::Backlog,
            ..Task::new(sun_core::WorkspaceId::MAIN, "Review the web header")
        };
        tx.insert_task(&named).await.unwrap();
        let run = queue_run(&mut tx, named.id, "created").await.unwrap();
        assert_eq!(tx.get_run(run).await.unwrap().repo_ids, vec![web.id]);
    }
}
