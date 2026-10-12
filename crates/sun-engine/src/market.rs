//! Marketplace placement: a task leased on Sun Cloud runs
//! only on the seller's host that joined with the task's single-use token,
//! inside the lease's reset VM; that host takes no other task. Kept as
//! settings (`task_market.<task>` = `{lease_id, host_id}`,
//! `host_market.<host>` = task id), read by the queue on each pump.
//!
//! **Lease before start:** [`Engine::hold_for_lease`] holds the task first
//! (`lease_id: null`), so no free buyer host starts it while Sun Cloud
//! answers; the lease then fills the hold ([`Engine::lease_task`]) or
//! [`Engine::release_lease_hold`] gives the task back to the queue. The
//! executor refuses to start a held or leased run on any other host, which
//! closes the gap between the queue reading and starting a run.
//!
//! **Ecosystem policy** (setting `market_policy`): marketplace
//! runtimes allowed for every workflow, none, or chosen workflows; checked
//! whenever a task is held or leased. **Per-secret consent:** a leased task's
//! sessions and pushes get only the ecosystem secrets the buyer approved
//! ([`TaskLease::secrets`], [`secret_withheld`]).
//!
//! **Failover**: a workflow chain entry `market:<adapter>` is the
//! marketplace's `<adapter>`. When the queue reaches it (every runtime before
//! it is cooling) it holds the task and journals [`MARKET_FAILOVER`]; the API
//! (which owns the join token and Sun Cloud) leases it, or drops the hold.
//! A running step that hits a limit does the same (`failover`), and its
//! hand-off moves the run to the seller's host once it joins.

use std::sync::Arc;

use serde::{Deserialize, Serialize};
use sun_core::model::{OutboxItem, OutboxStatus, Repo};
use sun_core::{
    Error, ExecEnvSpec, HostId, IdemKey, Marketplace, OutboxId, Result, RunId, RunStatus, TaskId,
    TaskStatus,
};
use sun_store::{
    EventLog, NewEvent, OutboxRepo, RepoRepo, RunRepo, SettingRepo, TaskRepo, Tx, WorkflowRepo,
};
use tokio::sync::broadcast::error::RecvError;

use crate::Engine;

/// Chain entry prefix naming a marketplace runtime: `market:claude-code`.
pub const MARKET_PREFIX: &str = "market:";

/// Event on the run whose task the queue failed over to the marketplace:
/// `{task_id, runtime, max_price_points}`.
pub const MARKET_FAILOVER: &str = "market_failover";

const POLICY_KEY: &str = "market_policy";

/// Pseudo-mod the engine's outbox addresses Sun Cloud's marketplace by:
/// `sun_cloud.release_lease` `{lease_id}` ends a lease no work ran on and
/// refunds its escrow. `sun serve` routes it to its [`Marketplace`].
pub const MARKET_MOD: &str = "sun_cloud";
/// The method of [`MARKET_MOD`] that releases a lease.
pub const RELEASE_LEASE: &str = "release_lease";

/// A task's marketplace lease, and the seller's host once it joined.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskLease {
    /// Sun Cloud's lease id; `None` while held for a lease being made.
    pub lease_id: Option<String>,
    /// The seller's host enrolled with the task's token.
    pub host_id: Option<HostId>,
    /// The leased runtime adapter; its sessions run it, whatever the chain says.
    #[serde(default)]
    pub runtime: Option<String>,
    /// Ecosystem secrets the buyer allowed this lease, by name; every other
    /// one is withheld from the seller's machine.
    #[serde(default)]
    pub secrets: Vec<String>,
    /// The task's own `exec_env` override the lease's VM replaced, put back
    /// when the lease is released ([`release`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub buyer_exec_env: Option<serde_json::Value>,
}

/// Which workflows may run on marketplace runtimes.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MarketAllow {
    /// Every workflow (the default).
    #[default]
    All,
    /// None: the marketplace is blocked.
    None,
    /// Only [`MarketPolicy::workflows`].
    Workflows,
}

/// The ecosystem's marketplace policy.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MarketPolicy {
    /// Who may use marketplace runtimes.
    #[serde(default)]
    pub allow: MarketAllow,
    /// Workflow names allowed when `allow` is `workflows`.
    #[serde(default)]
    pub workflows: Vec<String>,
    /// Most a failover lease pays per task, in points; unset = chains never
    /// fail over to the marketplace on their own.
    #[serde(default)]
    pub failover_max_price_points: Option<i64>,
}

impl MarketPolicy {
    fn validate(&self) -> Result<()> {
        let names_ok = self.workflows.len() <= 200
            && self
                .workflows
                .iter()
                .all(|w| !w.trim().is_empty() && w.len() <= 200);
        let price_ok = self
            .failover_max_price_points
            .is_none_or(|p| (1..=1_000_000_000).contains(&p));
        match names_ok && price_ok {
            true => Ok(()),
            false => Err(Error::InvalidInput(
                "workflows: at most 200 names of 1–200 characters; \
                 failover_max_price_points: 1–1000000000"
                    .into(),
            )),
        }
    }

    /// True when workflow `name` may run on marketplace runtimes.
    pub fn allows(&self, name: &str) -> bool {
        match self.allow {
            MarketAllow::All => true,
            MarketAllow::None => false,
            MarketAllow::Workflows => self.workflows.iter().any(|w| w == name),
        }
    }
}

fn task_key(task: TaskId) -> String {
    format!("task_market.{task}")
}

fn host_key(host: HostId) -> String {
    format!("host_market.{host}")
}

/// The lease of `task`, if it is leased.
pub(crate) async fn task_lease(tx: &mut Tx, task: TaskId) -> Result<Option<TaskLease>> {
    let value = tx.setting(&task_key(task)).await?;
    Ok(value.and_then(|v| serde_json::from_value(v).ok()))
}

/// True when `host` joined for a marketplace lease (it runs nothing else).
pub(crate) async fn is_market_host(tx: &mut Tx, host: HostId) -> Result<bool> {
    Ok(tx.setting(&host_key(host)).await?.is_some())
}

/// True when secret `name` must not reach `task`'s machine: the task is held
/// or leased on the marketplace and the buyer didn't allow that secret.
pub(crate) async fn secret_withheld(tx: &mut Tx, task: TaskId, name: &str) -> Result<bool> {
    Ok(task_lease(tx, task)
        .await?
        .is_some_and(|lease| !lease.secrets.iter().any(|s| s == name)))
}

pub(crate) async fn market_policy(tx: &mut Tx) -> Result<MarketPolicy> {
    let value = tx.setting(POLICY_KEY).await?;
    Ok(value
        .and_then(|v| serde_json::from_value(v).ok())
        .unwrap_or_default())
}

/// `InvalidInput` unless `task` hasn't started (`backlog` or `queued`).
async fn not_started(tx: &mut Tx, task: TaskId) -> Result<()> {
    let status = tx.get_task(task).await?.status;
    if !matches!(status, TaskStatus::Backlog | TaskStatus::Todo) {
        return Err(Error::InvalidInput(format!(
            "only a task that hasn't started can be leased; this one is {}",
            status.as_str()
        )));
    }
    Ok(())
}

/// `Forbidden` when the ecosystem policy keeps `task`'s workflow off the marketplace.
async fn allowed(tx: &mut Tx, task: TaskId) -> Result<()> {
    let Some(workflow) = tx.get_task(task).await?.workflow_id else {
        return Err(Error::InvalidInput(
            "a task without a workflow never runs, so it can't be leased".into(),
        ));
    };
    let workflow = tx.get_workflow(workflow).await?.name;
    if !market_policy(tx).await?.allows(&workflow) {
        return Err(Error::Forbidden(format!(
            "The ecosystem policy keeps workflow `{workflow}` off marketplace runtimes"
        )));
    }
    Ok(())
}

/// Why `task`'s running run can't move to a marketplace runtime mid-step
/// (failover's hand-off), if it can't: the move needs the policy's failover
/// price cap, a workflow the policy allows, no lease yet, and a repo (the
/// work travels as a pushed branch; a task folder has nowhere to go).
pub(crate) async fn unmovable(tx: &mut Tx, task: TaskId) -> Result<Option<&'static str>> {
    Ok(
        if market_policy(tx).await?.failover_max_price_points.is_none() {
            Some("the marketplace policy sets no failover price cap")
        } else if tx
            .runs_of_task(task)
            .await?
            .pop()
            .is_none_or(|r| r.repo_ids.is_empty())
        {
            Some("a task without a repo can't move hosts mid-run")
        } else if allowed(tx, task).await.is_err() {
            Some("the marketplace policy keeps this workflow off")
        } else if task_lease(tx, task).await?.is_some() {
            Some("the task is on the marketplace already")
        } else {
            None
        },
    )
}

/// Holds `task` (one that hasn't started) for `terms` in the caller's transaction.
pub(crate) async fn hold(tx: &mut Tx, task: TaskId, terms: TaskLease) -> Result<()> {
    not_started(tx, task).await?;
    hold_any(tx, task, terms).await
}

/// Holds `task` whatever its status: a running run's mid-step failover too.
async fn hold_any(tx: &mut Tx, task: TaskId, terms: TaskLease) -> Result<()> {
    allowed(tx, task).await?;
    if task_lease(tx, task).await?.is_some() {
        return Err(Error::InvalidInput("this task is already leased".into()));
    }
    let hold = TaskLease {
        lease_id: None,
        host_id: None,
        ..terms
    };
    tx.put_setting(&task_key(task), &serde_json::json!(hold))
        .await?;
    Ok(())
}

/// The failover to `market:<runtime>` of `run`: holds its task and journals
/// [`MARKET_FAILOVER`] in one transaction. A queued run is the queue's (it
/// then starts on the seller's host); a running one is a mid-step hand-off
/// (`failover`, which moves the run once the seller's host joins).
/// `Ok(false)`: the policy has no failover price cap or keeps the workflow
/// off, or the task is held already; the run keeps waiting for its own runtimes.
pub(crate) async fn fail_over(tx: &mut Tx, run: RunId, runtime: &str) -> Result<bool> {
    let Some(max_price_points) = market_policy(tx).await?.failover_max_price_points else {
        return Ok(false);
    };
    let run_row = tx.get_run(run).await?;
    let task = run_row.task_id;
    let terms = TaskLease {
        runtime: Some(runtime.to_owned()),
        ..TaskLease::default()
    };
    let held = match run_row.status {
        RunStatus::Running => hold_any(tx, task, terms).await,
        _ => hold(tx, task, terms).await,
    };
    match held {
        Ok(()) => {}
        Err(Error::InvalidInput(_) | Error::Forbidden(_)) => return Ok(false),
        Err(e) => return Err(e),
    }
    tx.append_event(NewEvent::new(
        run,
        None,
        MARKET_FAILOVER,
        serde_json::json!({
            "task_id": task, "runtime": runtime, "max_price_points": max_price_points,
        }),
    ))
    .await?;
    Ok(true)
}

/// Releases `task`'s filled lease in the caller's transaction (a mid-run
/// move whose checkpoint push failed): the task is off the marketplace again
/// and its own `exec_env` is back, and an outbox row (keyed by the lease, so
/// a retry never sends twice) has Sun Cloud end the lease and refund its
/// escrow after commit. The seller's host stays bound (it runs nothing else)
/// until Sun Cloud stops it. `Ok(true)`: a row was enqueued.
pub(crate) async fn release(tx: &mut Tx, run: RunId, task: TaskId) -> Result<bool> {
    let Some(TaskLease {
        lease_id: Some(lease_id),
        buyer_exec_env,
        ..
    }) = task_lease(tx, task).await?
    else {
        return Ok(false);
    };
    tx.put_setting(&task_key(task), &serde_json::Value::Null)
        .await?;
    tx.put_setting(
        &crate::executor::task_exec_env_key(task),
        &buyer_exec_env.unwrap_or_default(),
    )
    .await?;
    tx.enqueue(&OutboxItem {
        id: OutboxId::generate(),
        run_id: Some(run),
        idem_key: IdemKey::outside_run("market_release", &lease_id),
        target: format!("{MARKET_MOD}.{RELEASE_LEASE}"),
        payload: serde_json::json!({ "lease_id": lease_id }),
        status: OutboxStatus::Pending,
        result: None,
    })
    .await
    .map_err(Into::into)
}

impl Engine {
    /// The repos a lease's VM must reach: the task's latest run's, or,
    /// before it ran, the ones its text names as the queue will resolve
    /// them (`repo_resolve::run_repos`), in the workspace's list order.
    pub async fn lease_repos(&self, task: TaskId) -> Result<Vec<Repo>> {
        let mut tx = self.store.read().await?;
        let task = tx.get_task(task).await?;
        let ids = match tx.runs_of_task(task.id).await?.pop() {
            Some(run) => run.repo_ids,
            None => match task.workflow_id {
                None => Vec::new(),
                Some(workflow_id) => {
                    let Some(version) = tx.latest_workflow_version(workflow_id).await? else {
                        return Ok(Vec::new());
                    };
                    let workflow = sun_workflow::parse(&version.source_yaml)
                        .map_err(|_| Error::InvalidInput("the workflow doesn't parse".into()))?;
                    crate::repo_resolve::run_repos(&mut tx, &task, workflow_id, &workflow).await?
                }
            },
        };
        Ok(tx
            .repos(task.workspace_id)
            .await?
            .into_iter()
            .filter(|r| ids.contains(&r.id))
            .collect())
    }

    /// Holds a task that hasn't started for a lease being made on `terms`
    /// (runtime, allowed secrets): from now on no host starts it until
    /// [`Self::lease_task`] fills the hold (then only the seller's host does)
    /// or [`Self::release_lease_hold`] drops it. `Forbidden` when the
    /// ecosystem policy keeps the task's workflow off the marketplace.
    pub async fn hold_for_lease(&self, task: TaskId, terms: TaskLease) -> Result<()> {
        let mut tx = self.store.begin().await?;
        hold(&mut tx, task, terms).await?;
        Ok(tx.commit().await?)
    }

    /// Drops every hold no lease filled, for `sun serve` start: a kernel that
    /// died between a hold and Sun Cloud's answer would strand the task.
    // ponytail: scans the tasks of active (queued) runs only; a held backlog
    // task stays held until it is queued and this runs again.
    pub async fn clear_lease_holds(&self) -> Result<usize> {
        let mut tx = self.store.begin().await?;
        let mut cleared = Vec::new();
        for run in tx.active_runs().await? {
            if let Some(TaskLease { lease_id: None, .. }) = task_lease(&mut tx, run.task_id).await?
            {
                tx.put_setting(&task_key(run.task_id), &serde_json::Value::Null)
                    .await?;
                cleared.push(run.task_id);
            }
        }
        tx.commit().await?;
        // A running run's mid-step failover hands back to its own runtime.
        for task in &cleared {
            self.wake_handoffs(*task).await?;
        }
        Ok(cleared.len())
    }

    /// The ecosystem's marketplace policy.
    pub async fn market_policy(&self) -> Result<MarketPolicy> {
        market_policy(&mut self.store.read().await?).await
    }

    /// Replaces the ecosystem's marketplace policy.
    pub async fn set_market_policy(&self, policy: &MarketPolicy) -> Result<()> {
        policy.validate()?;
        let mut tx = self.store.begin().await?;
        tx.put_setting(POLICY_KEY, &serde_json::json!(policy))
            .await?;
        Ok(tx.commit().await?)
    }

    /// Drops a hold no lease filled (the lease failed): the task queues for
    /// the buyer's own hosts again. A filled lease stays.
    pub async fn release_lease_hold(&self, task: TaskId) -> Result<()> {
        let mut tx = self.store.begin().await?;
        if let Some(TaskLease { lease_id: None, .. }) = task_lease(&mut tx, task).await? {
            // `null` reads back as no lease (`task_lease`).
            tx.put_setting(&task_key(task), &serde_json::Value::Null)
                .await?;
        }
        tx.commit().await?;
        self.wake_handoffs(task).await
    }

    /// Wakes the hand-offs of `task`'s running run that wait for the
    /// marketplace (`failover`): the seller's host joined, or the lease failed.
    async fn wake_handoffs(&self, task: TaskId) -> Result<()> {
        let mut read = self.store.read().await?;
        let mut waiting = Vec::new();
        for run in read.active_runs().await? {
            if run.task_id == task && run.status == RunStatus::Running {
                let steps = read.steps_of_run(run.id).await?;
                waiting.extend(
                    steps
                        .into_iter()
                        .filter(|s| s.status == sun_core::StepStatus::HandingOff)
                        .map(|s| s.id),
                );
            }
        }
        drop(read);
        for step in waiting {
            self.wake(step).await;
        }
        Ok(())
    }

    /// Leases a task that hasn't started (`backlog` or `queued`), filling its
    /// hold if [`Self::hold_for_lease`] made one: every agent session of it
    /// runs in `vm` (a reset VM with a network allowlist) on the host that
    /// joins with its token ([`Self::bind_market_host`]); until then it waits
    /// in the queue.
    pub async fn lease_task(&self, task: TaskId, lease_id: &str, vm: &ExecEnvSpec) -> Result<()> {
        vm.validate()?;
        let ExecEnvSpec::Vm(spec) = vm else {
            return Err(Error::InvalidInput("a leased task runs in a vm".into()));
        };
        if spec.network.is_none() || spec.keep {
            return Err(Error::InvalidInput(
                "a leased task's vm needs a network allowlist and a reset".into(),
            ));
        }
        let mut tx = self.store.begin().await?;
        let held = task_lease(&mut tx, task).await?;
        // A hold may be a running run's mid-step failover; a lease without
        // one is only for a task that hasn't started.
        if held.is_none() {
            not_started(&mut tx, task).await?;
        }
        allowed(&mut tx, task).await?;
        if held.as_ref().is_some_and(|l| l.lease_id.is_some()) {
            return Err(Error::InvalidInput("this task is already leased".into()));
        }
        // The hold's terms stay, and the seller's host may have joined it already.
        let env_key = crate::executor::task_exec_env_key(task);
        let lease = TaskLease {
            lease_id: Some(lease_id.to_owned()),
            buyer_exec_env: tx.setting(&env_key).await?.filter(|v| !v.is_null()),
            ..held.unwrap_or_default()
        };
        tx.put_setting(&task_key(task), &serde_json::json!(lease))
            .await?;
        tx.put_setting(&env_key, &serde_json::json!(vm)).await?;
        Ok(tx.commit().await?)
    }

    /// The task's marketplace lease, if it is leased.
    pub async fn task_lease(&self, task: TaskId) -> Result<Option<TaskLease>> {
        task_lease(&mut self.store.read().await?, task).await
    }

    /// Runs forever (**Observer** on the event log): when a leased task ends
    /// (done, failed, cancelled), ends its lease so the seller's host stops
    /// and the escrow starts its dispute window. `sun serve` spawns it under
    /// Sun Cloud.
    // ponytail: live events only, and a direct call instead of an outbox row
    // (closing is idempotent on Sun Cloud); a lease whose task ended while
    // the kernel was down is closed by the buyer's Accept or Sun's timeout.
    pub async fn follow_leases(self, market: Arc<dyn Marketplace>) {
        let mut events = self.store.subscribe();
        loop {
            let event = match events.recv().await {
                Ok(event) => event,
                Err(RecvError::Lagged(_)) => continue,
                Err(RecvError::Closed) => return,
            };
            let ended = event.kind == "task_status"
                && matches!(
                    event.payload["to"].as_str(),
                    Some("done" | "failed" | "cancelled")
                );
            let task = serde_json::from_value::<TaskId>(event.payload["task_id"].clone());
            let (true, Ok(task)) = (ended, task) else {
                continue;
            };
            let lease_id = match self.task_lease(task).await {
                Ok(Some(TaskLease {
                    lease_id: Some(id), ..
                })) => id,
                Ok(_) => continue,
                Err(e) => {
                    eprintln!("sun: marketplace: reading the lease of {task}: {e}");
                    continue;
                }
            };
            if let Err(e) = market.close(&lease_id).await {
                eprintln!("sun: marketplace: closing lease {lease_id}: {e}");
            }
        }
    }

    /// Binds the seller's `host`, just enrolled with `task`'s token.
    pub async fn bind_market_host(&self, task: TaskId, host: HostId) -> Result<()> {
        let mut tx = self.store.begin().await?;
        let mut lease = task_lease(&mut tx, task)
            .await?
            .ok_or_else(|| Error::NotFound(format!("lease of task {task}")))?;
        lease.host_id = Some(host);
        tx.put_setting(&task_key(task), &serde_json::json!(lease))
            .await?;
        tx.put_setting(&host_key(host), &serde_json::json!(task))
            .await?;
        tx.commit().await?;
        self.wake_handoffs(task).await
    }
}
