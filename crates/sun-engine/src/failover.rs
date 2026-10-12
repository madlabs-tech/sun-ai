//! Runtime failover: a quota hit is a switch, not a failure.
//!
//! **Pattern:** **Chain of Responsibility** over the workflow's
//! `runtime.chain` (the first runtime that is not cooling down takes the
//! work) with a **Strategy** per `runtime.on_limit`. A child module of
//! `executor`, like `watchdog`, so it reuses the executor's private helpers.
//!
//! On a `LimitHit` from a running agent step, in the event's transaction:
//! 1. `running → limit_hit`; the runtime cools down until `reset_at` (or
//!    [`DEFAULT_COOLDOWN`]), journalled as `runtime_cooling`; the old session
//!    is stopped and no longer routed.
//! 2. The policy:
//!    - `switch`: `limit_hit → handing_off` to the first runtime of the chain
//!      that is not cooling down. All cooling: wait for the first reset.
//!    - `wait_until_reset`: a timer; at the reset, hand off to the same runtime.
//!    - `pause_and_notify`: the run goes `paused` with the reason (the
//!      `run_status` event is the notice). [`Engine::switch_runtime`] resumes it.
//!
//!    The policy resolves task > workflow > ecosystem > `switch`:
//!    the task's and the ecosystem's live in `kernel_setting`
//!    ([`Engine::set_on_limit`]).
//! 3. After commit ([`Effect::Handoff`]): the checkpoint commit
//!    `wip(sun): handoff from <runtime>` on the host (hooks skipped, nothing
//!    pushed), the hand-off pack (no LLM, `sun_context::build_handoff`), then
//!    `→ dispatched` on a new session of the target runtime in the **same
//!    worktree**. The step attempt is the same: nothing re-runs.
//!
//! A `market:<adapter>` chain entry takes over a limit hit under
//! `switch` when the run may move ([`crate::market::unmovable`]): the task is
//! held and `market_failover` journalled with the move; the API leases it.
//! The hand-off then waits until the seller's host joins
//! (`bind_market_host` wakes it), checkpoints and **pushes** the task branch
//! from the old host, and re-places the run on the seller's host
//! (`run_moved.<run>`), whose `prepare_worktree` checks the branch out; the
//! new session runs the leased adapter with the pack as usual. A refused
//! lease or a failed push waits for `from`'s reset, then hands back to it.
//! A step never starts on a `market:` entry; a queued run fails over in the
//! queue instead.
//!
//! The pack's `transcript_ref` is `blob:<hash>` of the old session's whole
//! conversation (redacted), stored as the `handoff` event's blob, so the new
//! agent fetches it with `sun.get`.
//!
//! Gates: the `gate` steps that `need` the agent step run once it finishes,
//! so every gate re-checks the work of the runtime that finished it.
//! With `runtime.review_after_failover: [<who>]`, an agent step that ran on
//! more than one runtime then waits in `waiting_approval` for those
//! approvers (`failover_review`) before it succeeds.
//!
//! Switch back (opt-in `runtime.switch_back`): once `chain[0]`'s
//! cooldown is over, a step running elsewhere hands back to it at the next
//! tool result (a safe point), reason `switch_back`.
//!
//! A manual switch uses the same hand-off: `running → handing_off
//! → dispatched`. (Journals from before 1.12b may show `interrupted` instead;
//! resume still finishes those.)
//!
//! Crash safety: `handoff_started` is journalled with the move, so resume
//! redoes a hand-off cut short (the checkpoint only commits a dirty tree).
//!
//! [`Engine::switch_runtime`]: super::Engine::switch_runtime
//! [`Engine::set_on_limit`]: super::Engine::set_on_limit

use std::collections::HashMap;
use std::time::Duration;

use chrono::Utc;
use serde::{Deserialize, Serialize};
use serde_json::json;
use sun_context::{GateResult, GitState, HandoffFrom, HandoffInput, HandoffReason, build_handoff};
use sun_core::host_protocol::{Capabilities, ExecRequest};
use sun_core::model::{self, EventRecord, RuntimeSource, Step};
use sun_core::redact::redact;
use sun_core::{
    Error, HandoffPackId, HostId, Result, RunId, RunStatus, StepId, StepKey, StepStatus, SunEvent,
    TaskId, Timestamp,
};
use sun_store::{
    EventLog, HostRepo, NewEvent, RunRepo, RuntimeRepo, SecretRepo, SessionRepo, SettingRepo,
    TaskRepo, Tx,
};
use sun_workflow::{OnLimit, QuestionKind, Workflow};

use super::dispatch::Resume;
use super::{
    Effect, Executor, Hook, Settled, append, decision, move_run, move_step, notes_of, step_brief,
};
use crate::market::{MARKET_PREFIX, TaskLease};
use crate::push::{SECRET_FOUND, SecretScan};

/// Cooldown of a limit that carries no `reset_at`.
// ponytail: one fixed value; make it per runtime once limits are profiled.
pub(super) const DEFAULT_COOLDOWN: Duration = Duration::from_secs(30 * 60);

/// A runtime cools down until `until` (timeline + restart-safe record).
const RUNTIME_COOLING: &str = "runtime_cooling";
/// A hand-off was decided; the payload is a [`Handoff`].
pub(super) const HANDOFF_STARTED: &str = "handoff_started";
/// A hand-off finished: `{from, to, reason, checkpoint, pack}`.
pub(crate) const HANDOFF: &str = "handoff";
/// A runtime of the chain was passed over: `{runtime, reason}`.
const RUNTIME_SKIPPED: &str = "runtime_skipped";
/// A hand-off to a marketplace runtime couldn't push its checkpoint:
/// `{step_id, to, error}`; its lease is released (Sun Cloud refunds it) and
/// the step waits for `from`'s reset or hands back to it.
const MARKET_MOVE_FAILED: &str = "market_move_failed";
/// An agent step that failed over waits for a human: `{step_id, runtimes, who}`.
const FAILOVER_REVIEW: &str = "failover_review";

/// `kernel_setting` key of a task's own `on_limit`, or (`None`) the ecosystem's.
fn on_limit_key(task: Option<TaskId>) -> String {
    match task {
        Some(id) => format!("on_limit:task:{id}"),
        None => "on_limit".to_owned(),
    }
}

/// `on_limit` of a task's run: task > workflow > ecosystem > `switch`.
async fn resolve_on_limit(tx: &mut Tx, workflow: &Workflow, task: TaskId) -> Result<OnLimit> {
    let stored = |v: Option<serde_json::Value>| v.and_then(|v| serde_json::from_value(v).ok());
    if let Some(own) = stored(tx.setting(&on_limit_key(Some(task))).await?) {
        return Ok(own);
    }
    if let Some(set) = workflow.runtime.as_ref().and_then(|p| p.on_limit) {
        return Ok(set);
    }
    Ok(stored(tx.setting(&on_limit_key(None)).await?).unwrap_or(OnLimit::Switch))
}

/// The run's events of `kind`, in order.
async fn events_of(tx: &mut Tx, run_id: RunId, kind: &str) -> Result<Vec<EventRecord>> {
    let (mut found, mut cursor) = (Vec::new(), 0);
    loop {
        let page = tx.events_after(Some(run_id), cursor, 500).await?;
        let Some(last) = page.last() else { break };
        cursor = last.seq;
        found.extend(page.into_iter().filter(|e| e.kind == kind));
    }
    Ok(found)
}

/// Which runtime ran a step attempt, and who paid.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RanOn {
    pub step_id: StepId,
    pub step_key: StepKey,
    pub runtime: String,
    /// `RuntimeSource` wire name; empty in older journals.
    pub source: String,
}

/// Every runtime that took over a step of `run_id`, from its
/// `session_assigned` events, in order; repeats of the same runtime in a row
/// (a resume, a context rotation) are merged.
pub(crate) async fn runtimes_of_run(tx: &mut Tx, run_id: RunId) -> Result<Vec<RanOn>> {
    let mut ran: Vec<RanOn> = Vec::new();
    for event in events_of(tx, run_id, super::kind::SESSION_ASSIGNED).await? {
        let (Some(step_key), Ok(step_id)) = (
            event.step_key,
            serde_json::from_value::<StepId>(event.payload["step_id"].clone()),
        ) else {
            continue;
        };
        let field = |key: &str| event.payload[key].as_str().unwrap_or_default().to_owned();
        let next = RanOn {
            step_id,
            step_key,
            runtime: field("adapter"),
            source: field("source"),
        };
        let repeat = ran
            .iter()
            .rev()
            .find(|r| r.step_id == step_id)
            .is_some_and(|r| r.runtime == next.runtime);
        if !repeat {
            ran.push(next);
        }
    }
    Ok(ran)
}

/// A hand-off to a marketplace runtime and where it stands (`market_move`).
enum MarketMove {
    /// The lease is filled and the seller's host joined: move there.
    To(HostId),
    /// The lease is being made, or `from` still cools down after a refusal.
    Wait,
    /// The lease was refused and `from` has reset: hand back to it.
    Back,
}

/// Where the work of one step attempt moves. Journalled as `handoff_started`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(super) struct Handoff {
    pub step_id: StepId,
    pub from: String,
    pub to: String,
    pub reason: HandoffReason,
    pub reset_at: Option<Timestamp>,
}

/// Runtime cooldowns, by chain name. A cache of `runtime_state`:
/// written there in the same transaction, loaded at engine start.
#[derive(Debug, Default)]
pub(super) struct Failover {
    // ponytail: keyed by chain name across the deployment (and loaded from
    // every workspace's `runtime_state` by name), so one workspace's limit hit
    // cools that name for all; key by (workspace, name) or runtime id when
    // workspaces bring their own accounts.
    cooling: HashMap<String, Timestamp>,
}

impl Failover {
    /// The cooldowns stored in `runtime_state` (they survive a restart).
    pub(super) async fn load(tx: &mut Tx) -> Result<Self> {
        Ok(Self {
            cooling: tx.cooldowns().await?.into_iter().collect(),
        })
    }

    fn cooling_until(&self, runtime: &str, now: Timestamp) -> Option<Timestamp> {
        self.cooling
            .get(runtime)
            .copied()
            .filter(|until| *until > now)
    }

    /// The first runtime of `chain` that is ready (not in `unready`) and not
    /// cooling down.
    fn pick(
        &self,
        chain: &[String],
        now: Timestamp,
        unready: &HashMap<String, String>,
    ) -> Option<String> {
        chain
            .iter()
            .find(|r| !unready.contains_key(*r) && self.cooling_until(r, now).is_none())
            .cloned()
    }

    /// The earliest end of a cooldown in `chain`.
    fn first_reset(&self, chain: &[String], now: Timestamp) -> Option<Timestamp> {
        chain
            .iter()
            .filter_map(|r| self.cooling_until(r, now))
            .min()
    }
}

/// The runtime chain of a workflow; without `runtime:` the engine default.
pub(crate) fn chain(workflow: &Workflow, default: &str) -> Vec<String> {
    workflow
        .runtime
        .as_ref()
        .map_or_else(|| vec![default.to_owned()], |p| p.chain.clone())
}

/// Why `adapter` can't run on a host with `capabilities`, if it can't.
pub(super) fn unready_reason(capabilities: &Capabilities, adapter: &str) -> Option<String> {
    let Some(runtime) = capabilities.runtimes.iter().find(|r| r.adapter == adapter) else {
        return Some(format!("`{adapter}` is not on the host"));
    };
    let readiness = &runtime.readiness;
    let problem = if !readiness.installed {
        "is not installed"
    } else if !readiness.logged_in {
        "is not logged in"
    } else {
        return None;
    };
    Some(match &readiness.detail {
        Some(detail) => format!("`{adapter}` {problem}: {detail}"),
        None => format!("`{adapter}` {problem}"),
    })
}

impl super::Engine {
    /// Sets (`Some`) or clears (`None`) what a runtime limit does for one
    /// task, or (`task: None`) for the ecosystem. A task's setting
    /// beats its workflow's `runtime.on_limit`, which beats the ecosystem's.
    pub async fn set_on_limit(
        &self,
        task: Option<TaskId>,
        on_limit: Option<OnLimit>,
    ) -> Result<()> {
        let mut tx = self.store.begin().await?;
        if let Some(id) = task {
            tx.get_task(id).await?;
        }
        tx.put_setting(&on_limit_key(task), &json!(on_limit))
            .await?;
        Ok(tx.commit().await?)
    }

    /// The `on_limit` set for one task or (`None`) the ecosystem, if any.
    pub async fn on_limit(&self, task: Option<TaskId>) -> Result<Option<OnLimit>> {
        let mut read = self.store.read().await?;
        let value = read.setting(&on_limit_key(task)).await?;
        Ok(value.and_then(|v| serde_json::from_value(v).ok()))
    }
}

/// The failover decision's answer that pauses instead of switching.
const PAUSE: &str = "pause";

fn after(until: Timestamp) -> Duration {
    (until - Utc::now()).to_std().unwrap_or_default()
}

impl Executor {
    /// The runtime a fresh agent step starts on: the first of its chain that
    /// is ready and not cooling down (else the first).
    pub(super) async fn first_runtime(&mut self, tx: &mut Tx, step: &Step) -> Result<String> {
        let run = tx.get_run(step.run_id).await?;
        // A leased task runs the runtime it rented, on the seller's host.
        let lease = crate::market::task_lease(tx, run.task_id).await?;
        if let Some(runtime) = lease.and_then(|l| l.runtime) {
            return Ok(runtime);
        }
        let mut chain = chain(&*self.workflow(tx, &run).await?, &self.config.adapter);
        let workspace = tx.workspace_of_run(run.id).await?;
        // A `route` decision puts its runtime first, if it is one of the
        // workspace's.
        if let Some(to) = super::decision::routed(tx, run.id).await?
            && (chain.contains(&to) || tx.runtime_by_name(workspace, &to).await?.is_some())
        {
            chain.retain(|r| *r != to);
            chain.insert(0, to);
        }
        Ok(self
            .pick_ready(tx, step, &chain, false)
            .await?
            .unwrap_or_else(|| chain[0].clone()))
    }

    /// The first runtime of `chain` that is ready on the agent host (its
    /// adapter installed and logged in, per the host's last `hello`) and not
    /// cooling down. Each unready runtime passed over is journalled as
    /// `runtime_skipped {runtime, reason}`. A host that reported no
    /// capabilities is trusted. A `market:` entry is ready only for a limit
    /// hit (`market`) whose run may move there ([`crate::market::unmovable`]).
    async fn pick_ready(
        &mut self,
        tx: &mut Tx,
        step: &Step,
        chain: &[String],
        market: bool,
    ) -> Result<Option<String>> {
        let capabilities = match tx.get_host(self.host(step.run_id)).await {
            Ok(host) => serde_json::from_value::<Capabilities>(host.capabilities).ok(),
            Err(_) => None,
        };
        let mut unready = HashMap::new();
        if chain.iter().any(|n| n.starts_with(MARKET_PREFIX)) {
            let why = match market {
                true => {
                    let task = tx.get_run(step.run_id).await?.task_id;
                    crate::market::unmovable(tx, task).await?
                }
                false => Some("a marketplace runtime only takes over a step that hit a limit"),
            };
            for name in chain.iter().filter(|n| n.starts_with(MARKET_PREFIX)) {
                if let Some(why) = why {
                    unready.insert(name.clone(), format!("a marketplace runtime: {why}"));
                }
            }
        }
        if let Some(capabilities) = &capabilities {
            let workspace = tx.workspace_of_run(step.run_id).await?;
            // A `market:` runtime runs on the seller's host, not this one.
            for name in chain.iter().filter(|n| !n.starts_with(MARKET_PREFIX)) {
                let adapter = match tx.runtime_by_name(workspace, name).await? {
                    Some(row) => row.adapter,
                    None => name.clone(),
                };
                if let Some(why) = unready_reason(capabilities, &adapter) {
                    unready.insert(name.clone(), why);
                }
            }
        }
        let picked = self.failover.pick(chain, Utc::now(), &unready);
        for name in chain.iter().take_while(|n| Some(*n) != picked.as_ref()) {
            if let Some(why) = unready.get(name) {
                let payload = json!({ "runtime": name, "reason": why });
                append(tx, step, RUNTIME_SKIPPED, payload).await?;
            }
        }
        Ok(picked)
    }

    /// The runtime (chain name) of the step's latest session
    /// (`step.runtime_id`); a step that never had one gets the first ready.
    pub(super) async fn runtime_of(&mut self, tx: &mut Tx, step: &Step) -> Result<String> {
        match tx.get_step(step.id).await?.runtime_id {
            Some(id) => Ok(tx.get_runtime(id).await?.name),
            None => self.first_runtime(tx, step).await,
        }
    }

    /// A running agent step whose session is gone (lost host, host restart):
    /// `running → interrupted`, then a hand-off to the same runtime, which
    /// resumes the CLI's session first (`dispatch`).
    pub(super) async fn lose_session(
        &mut self,
        tx: &mut Tx,
        step: &mut Step,
        effects: &mut Vec<Effect>,
    ) -> Result<()> {
        move_step(tx, step, StepStatus::Interrupted, Some("session lost")).await?;
        let runtime = self.runtime_of(tx, step).await?;
        let handoff = Handoff {
            step_id: step.id,
            from: runtime.clone(),
            to: runtime,
            reason: HandoffReason::HostLost,
            reset_at: None,
        };
        self.stop_step_sessions(step.id, effects);
        append(tx, step, HANDOFF_STARTED, json!(handoff)).await?;
        effects.push(Effect::Handoff(handoff));
        Ok(())
    }

    /// Hook: `step` just moved `running → limit_hit`.
    pub(super) async fn on_limit(
        &mut self,
        tx: &mut Tx,
        step: &mut Step,
        reset_at: Option<Timestamp>,
        effects: &mut Vec<Effect>,
    ) -> Result<()> {
        let from = self.runtime_of(tx, step).await?;
        let until = reset_at.unwrap_or_else(|| {
            Utc::now() + chrono::Duration::from_std(DEFAULT_COOLDOWN).unwrap_or_default()
        });
        self.failover.cooling.insert(from.clone(), until);
        let workspace = tx.workspace_of_run(step.run_id).await?;
        let runtime = tx
            .upsert_runtime(workspace, &from, &from, RuntimeSource::ByoRuntime)
            .await?;
        tx.set_cooldown(runtime.id, Some(until)).await?;
        append(
            tx,
            step,
            RUNTIME_COOLING,
            json!({ "runtime": from, "until": until }),
        )
        .await?;
        self.stop_step_sessions(step.id, effects);
        self.limit_policy(tx, step, from, None, effects).await
    }

    /// A `limit_hit` step's timer fired (or resume found one): try again.
    pub(super) async fn retry_limit(
        &mut self,
        tx: &mut Tx,
        step: &mut Step,
        effects: &mut Vec<Effect>,
    ) -> Result<()> {
        let from = self.runtime_of(tx, step).await?;
        self.limit_policy(tx, step, from, None, effects).await
    }

    /// Applies `runtime.on_limit` to a `limit_hit` step of a running run.
    /// Under `switch`, a decision backend first picks the runtime, or a
    /// pause; `judged` is its answer once it gave one.
    async fn limit_policy(
        &mut self,
        tx: &mut Tx,
        step: &mut Step,
        from: String,
        judged: Option<&Settled>,
        effects: &mut Vec<Effect>,
    ) -> Result<()> {
        let run = tx.get_run(step.run_id).await?;
        if run.status != RunStatus::Running {
            return Ok(()); // paused: a human switches or resumes
        }
        let workflow = self.workflow(tx, &run).await?;
        let mut chain = chain(&workflow, &self.config.adapter);
        let now = Utc::now();
        let reset_at = self.failover.cooling_until(&from, now);
        let on_limit = resolve_on_limit(tx, &workflow, run.task_id).await?;
        let handoff = |to: String| Handoff {
            step_id: step.id,
            from: from.clone(),
            to,
            reason: HandoffReason::LimitHit,
            reset_at,
        };
        let target = match on_limit {
            OnLimit::Switch => {
                match judged.and_then(Settled::choice) {
                    Some(PAUSE) => {
                        let why = format!(
                            "runtime `{from}` hit its limit and the decision engine chose to \
                             pause: switch runtime, or resume after the reset"
                        );
                        return move_run(tx, run.id, RunStatus::Paused, Some(&why)).await;
                    }
                    // The pick goes first; readiness and cooldowns still apply.
                    Some(to) if chain.iter().any(|r| r == to) => {
                        chain.retain(|r| r != to);
                        chain.insert(0, to.to_owned());
                    }
                    _ => {}
                }
                if judged.is_none()
                    && let Some(client) = self.decisions.get()
                    && let Some(hook) = self.limit_hook(&chain, &from, reset_at, now)
                {
                    let step_id = step.id;
                    effects.push(Effect::Ask {
                        step_id,
                        hook,
                        client,
                    });
                    return Ok(());
                }
                self.pick_ready(tx, step, &chain, true).await?
            }
            OnLimit::WaitUntilReset => reset_at.is_none().then(|| from.clone()),
            OnLimit::PauseAndNotify => {
                let when = reset_at.map_or_else(String::new, |t| format!(" (resets {t})"));
                let why = format!(
                    "runtime `{from}` hit its limit{when}: switch runtime, or resume after the reset"
                );
                return move_run(tx, run.id, RunStatus::Paused, Some(&why)).await;
            }
        };
        // A marketplace runtime: the task is held for a lease (the API makes
        // it), and the hand-off waits for the seller's host (`market_move`).
        let target = match target {
            Some(to) if to.starts_with(MARKET_PREFIX) => {
                let adapter = &to[MARKET_PREFIX.len()..];
                let held = crate::market::fail_over(tx, run.id, adapter).await?;
                held.then_some(to)
            }
            other => other,
        };
        match target {
            Some(to) => self.begin_handoff(tx, step, handoff(to), effects).await,
            // Everything is cooling: wake at the first reset and decide again.
            None => {
                let until = match on_limit {
                    OnLimit::WaitUntilReset => reset_at,
                    _ => self.failover.first_reset(&chain, now),
                };
                effects.push(Effect::Timer {
                    step_id: step.id,
                    after: until.map_or(DEFAULT_COOLDOWN, after),
                });
                Ok(())
            }
        }
    }

    /// "Which runtime next, or pause?" over the chain's runtimes that are
    /// not cooling down. Rule: the first of them. `None`: none to pick.
    fn limit_hook(
        &self,
        chain: &[String],
        from: &str,
        reset_at: Option<Timestamp>,
        now: Timestamp,
    ) -> Option<Hook> {
        let free: Vec<&String> = chain
            .iter()
            .filter(|r| *r != from && self.failover.cooling_until(r, now).is_none())
            .collect();
        let first = free.first()?;
        let mut criteria = serde_json::Map::new();
        for runtime in &free {
            criteria.insert(
                (*runtime).clone(),
                json!(format!("switch to `{runtime}` now")),
            );
        }
        let resets = reset_at.map_or_else(String::new, |t| format!(" (it resets {t})"));
        let pause = format!("pause until a person switches, or `{from}` resets{resets}");
        criteria.insert(PAUSE.into(), json!(pause));
        Some(Hook::new(
            decision::FAILOVER,
            QuestionKind::Choice,
            "An agent's runtime hit its usage limit mid-step. Switch to another runtime now, or \
             pause the run?",
            json!(criteria),
            json!({ "from": from, "resets_at": reset_at, "chain": chain }),
            json!({ "choice": first }),
        ))
    }

    /// The decision engine answered the limit question ([`Self::limit_hook`]).
    pub(super) async fn on_limit_judged(
        &mut self,
        step_id: StepId,
        settled: Settled,
    ) -> Result<()> {
        let mut tx = self.store.begin().await?;
        let mut step = tx.get_step(step_id).await?;
        let mut effects = Vec::new();
        if step.status == StepStatus::LimitHit {
            settled
                .journal(&mut tx, step.run_id, Some(&step.step_key))
                .await?;
            let from = self.runtime_of(&mut tx, &step).await?;
            self.limit_policy(&mut tx, &mut step, from, Some(&settled), &mut effects)
                .await?;
        }
        tx.commit().await?;
        self.apply(effects).await
    }

    /// Moves `step` out of its session and schedules the hand-off.
    pub(super) async fn begin_handoff(
        &mut self,
        tx: &mut Tx,
        step: &mut Step,
        handoff: Handoff,
        effects: &mut Vec<Effect>,
    ) -> Result<()> {
        let why = format!("hand-off from `{}` to `{}`", handoff.from, handoff.to);
        match step.status {
            StepStatus::LimitHit | StepStatus::Running => {
                move_step(tx, step, StepStatus::HandingOff, Some(&why)).await?;
            }
            other => {
                return Err(Error::InvalidInput(format!(
                    "step `{}` is {other}; only a running or limit_hit step can switch",
                    step.step_key
                )));
            }
        }
        self.stop_step_sessions(step.id, effects);
        append(tx, step, HANDOFF_STARTED, json!(handoff)).await?;
        effects.push(Effect::Handoff(handoff));
        Ok(())
    }

    /// Manual switch: the same hand-off as automatic failover.
    /// `to` must be in the run's chain; a paused run resumes.
    pub(super) async fn switch(
        &mut self,
        run_id: RunId,
        step_key: StepKey,
        to: String,
    ) -> Result<()> {
        let mut tx = self.store.begin().await?;
        let run = tx.get_run(run_id).await?;
        let chain = chain(&*self.workflow(&mut tx, &run).await?, &self.config.adapter);
        if !chain.contains(&to) {
            return Err(Error::InvalidInput(format!(
                "runtime `{to}` is not in the chain [{}]",
                chain.join(", ")
            )));
        }
        let mut step = crate::state::latest(tx.steps_of_run(run_id).await?)
            .remove(&step_key)
            .ok_or_else(|| Error::NotFound(format!("step `{step_key}` of run {run_id}")))?;
        match run.status {
            RunStatus::Paused => {
                move_run(
                    &mut tx,
                    run_id,
                    RunStatus::Running,
                    Some("runtime switched"),
                )
                .await?;
            }
            RunStatus::Running => {}
            other => return Err(Error::InvalidInput(format!("run {run_id} is {other}"))),
        }
        let from = self.runtime_of(&mut tx, &step).await?;
        let handoff = Handoff {
            step_id: step.id,
            reset_at: self.failover.cooling_until(&from, Utc::now()),
            from,
            to,
            reason: HandoffReason::ManualSwitch,
        };
        let mut effects = Vec::new();
        self.begin_handoff(&mut tx, &mut step, handoff, &mut effects)
            .await?;
        tx.commit().await?;
        self.apply(effects).await
    }

    /// Hook: a fresh agent `event` of a `running` step. With
    /// `runtime.switch_back`, at a tool result after `chain[0]`'s cooldown
    /// (one that ended after the step started) is over, a step on another
    /// runtime hands back to `chain[0]`.
    pub(super) async fn switch_back_if_due(
        &mut self,
        tx: &mut Tx,
        step: &mut Step,
        event: &SunEvent,
        effects: &mut Vec<Effect>,
    ) -> Result<()> {
        let now = Utc::now();
        let since = step.started_at.unwrap_or(now);
        let over = |until: &Timestamp| *until > since && *until <= now;
        // Cheap test first: this runs on every agent event.
        if !matches!(event, SunEvent::ToolResult { .. })
            || !self.failover.cooling.values().any(over)
        {
            return Ok(());
        }
        let run = tx.get_run(step.run_id).await?;
        let workflow = self.workflow(tx, &run).await?;
        let Some(preferred) = workflow
            .runtime
            .as_ref()
            .filter(|p| p.switch_back)
            .and_then(|p| p.chain.first())
        else {
            return Ok(());
        };
        if !self.failover.cooling.get(preferred).is_some_and(over) {
            return Ok(());
        }
        let from = self.runtime_of(tx, step).await?;
        if from == *preferred {
            return Ok(());
        }
        let handoff = Handoff {
            step_id: step.id,
            from,
            to: preferred.clone(),
            reason: HandoffReason::SwitchBack,
            reset_at: None,
        };
        self.begin_handoff(tx, step, handoff, effects).await
    }

    /// Hook: an agent step's done claim held (`verifying`). With
    /// `runtime.review_after_failover`, a step that ran on more than one
    /// runtime moves `→ waiting_approval` for those approvers instead of
    /// succeeding; returns whether it did.
    pub(super) async fn review_after_failover(
        &mut self,
        tx: &mut Tx,
        step: &mut Step,
    ) -> Result<bool> {
        let run = tx.get_run(step.run_id).await?;
        let workflow = self.workflow(tx, &run).await?;
        let who = match &workflow.runtime {
            Some(policy) if !policy.review_after_failover.is_empty() => {
                policy.review_after_failover.clone()
            }
            _ => return Ok(false),
        };
        let mut runtimes: Vec<String> = Vec::new();
        for ran in runtimes_of_run(tx, step.run_id).await? {
            if ran.step_id == step.id && !runtimes.contains(&ran.runtime) {
                runtimes.push(ran.runtime);
            }
        }
        if runtimes.len() < 2 {
            return Ok(false);
        }
        let why = format!(
            "ran on {} after a failover: {} reviews it",
            runtimes.join(" → "),
            who.join(", ")
        );
        move_step(tx, step, StepStatus::WaitingApproval, Some(&why)).await?;
        let payload =
            json!({ "step_id": step.id, "runtimes": runtimes, "who": who, "summary": why });
        append(tx, step, FAILOVER_REVIEW, payload).await?;
        Ok(true)
    }

    /// Whether `step` (an agent step in `waiting_approval`) waits for a
    /// failover review rather than an answer to its own `sun.ask`.
    pub(super) async fn in_failover_review(&mut self, tx: &mut Tx, step: &Step) -> Result<bool> {
        if step.status != StepStatus::WaitingApproval {
            return Ok(false);
        }
        let last_of = |events: Vec<EventRecord>| {
            events
                .into_iter()
                .filter(|e| e.payload["step_id"] == json!(step.id))
                .map(|e| e.seq)
                .max()
        };
        let review = last_of(events_of(tx, step.run_id, FAILOVER_REVIEW).await?);
        let asked = last_of(events_of(tx, step.run_id, super::agent::QUESTION).await?);
        Ok(review > asked)
    }

    /// The hand-off a `handing_off`/`interrupted` step was in, if any (resume).
    pub(super) async fn pending_handoff(
        &mut self,
        tx: &mut Tx,
        step: &Step,
    ) -> Result<Option<Handoff>> {
        let mut found = None;
        let mut cursor = 0;
        loop {
            let page = tx.events_after(Some(step.run_id), cursor, 500).await?;
            let Some(last) = page.last() else { break };
            cursor = last.seq;
            for event in page.iter().filter(|e| e.kind == HANDOFF_STARTED) {
                let handoff: Handoff =
                    serde_json::from_value(event.payload.clone()).map_err(|e| {
                        Error::InvalidInput(format!("journal `{HANDOFF_STARTED}`: {e}"))
                    })?;
                if handoff.step_id == step.id {
                    found = Some(handoff);
                }
            }
        }
        Ok(found)
    }

    /// After commit: checkpoint, build the pack, start the new session.
    pub(super) async fn hand_off(&mut self, mut handoff: Handoff) -> Result<()> {
        let pending = |s: StepStatus| matches!(s, StepStatus::HandingOff | StepStatus::Interrupted);
        let step = self.store.read().await?.get_step(handoff.step_id).await?;
        if !pending(step.status) {
            return Ok(());
        }
        // To a marketplace runtime: the seller's host and the adapter it runs.
        let mut seller = None;
        if let Some(adapter) = handoff.to.strip_prefix(MARKET_PREFIX).map(str::to_owned) {
            match self.market_move(&step, &handoff).await? {
                MarketMove::To(host) => seller = Some((host, adapter)),
                MarketMove::Wait => return Ok(()),
                MarketMove::Back => handoff.to = handoff.from.clone(),
            }
        }
        // Host I/O first, with no transaction open.
        let secrets = self.store.read().await?.known_secrets().await?;
        let worktree = self.worktree(step.run_id).await;
        let scan = SecretScan::new(secrets.clone());
        let (git, checkpoint) = match &worktree {
            Ok(worktree) => match self
                .checkpoint(step.run_id, worktree, &handoff.from, &scan)
                .await
            {
                Ok(git) => (git, "ok".to_owned()),
                // The edits are still in the same worktree; carry on without git
                // (a secret in the change blocks the commit; the push blocks again).
                Err(e) => (None, e.to_string()),
            },
            Err(e) => (None, e.to_string()),
        };
        // The work travels to the seller's host as the pushed task branch,
        // which its `prepare_worktree` checks out. No push, no move: the
        // lease is released and, like a refused one, the step waits for
        // `from`'s reset or hands back to `from` (a later limit may fail
        // over to the marketplace again).
        if seller.is_some() {
            let pushed = match checkpoint.as_str() {
                "ok" => self.push_checkpoint(step.run_id).await,
                why => Err(why.to_owned()),
            };
            if let Err(error) = pushed {
                // No paid lease left idle: release it with the failure.
                let mut tx = self.store.begin().await?;
                let payload = json!({ "step_id": step.id, "to": handoff.to, "error": error });
                append(&mut tx, &step, MARKET_MOVE_FAILED, payload).await?;
                let task = tx.get_run(step.run_id).await?.task_id;
                let released = crate::market::release(&mut tx, step.run_id, task).await?;
                tx.commit().await?;
                if released {
                    Box::pin(self.apply(vec![Effect::WakeOutbox])).await?;
                }
                if self.wait_for_reset(&step, &handoff.from).await? {
                    return Ok(());
                }
                (seller, handoff.to) = (None, handoff.from.clone());
            }
        }
        let progress_md = match &worktree {
            Ok(worktree) => self.read_progress(step.run_id, worktree).await,
            Err(_) => String::new(),
        };

        let mut read = self.store.read().await?;
        let brief = step_brief(&mut read, step.run_id, step.step_key.clone()).await?;
        let (events, gates) = journal_of(&mut read, &step).await?;
        let notes = notes_of(&mut read, step.run_id).await?;
        drop(read);
        let secrets: Vec<&str> = secrets.iter().map(String::as_str).collect();
        // The old session's whole conversation, behind the pack's pointer.
        let transcript = redact(&sun_context::transcript(&events), &secrets);
        let transcript_hash = self.store.blobs().put(transcript.into_bytes()).await?;
        let transcript_ref = format!("blob:{}", transcript_hash.as_str());
        let from = HandoffFrom {
            runtime: handoff.from.clone(),
            reason: handoff.reason,
            reset_at: handoff.reset_at,
        };
        let pack = build_handoff(HandoffInput {
            brief: &brief,
            attempt: step.attempt,
            from: &from,
            events: &events,
            notes: &notes,
            progress_md: &progress_md,
            git: git.as_ref(),
            gates: &gates,
            transcript_ref: Some(&transcript_ref),
            known_secrets: &secrets,
        });

        let json = serde_json::to_vec(&pack).map_err(|e| Error::Protocol(e.to_string()))?;
        let pack_hash = self.store.blobs().put(json).await?;
        let mut tx = self.store.begin().await?;
        let mut current = tx.get_step(step.id).await?;
        if !pending(current.status) {
            return Ok(()); // cancelled meanwhile; dropping `tx` rolls back
        }
        // The transcript blob hangs off the event, so `sun.get` resolves it.
        let mut event = NewEvent::new(
            current.run_id,
            Some(current.step_key.clone()),
            HANDOFF,
            json!({
                "step_id": current.id,
                "from": handoff.from,
                "to": handoff.to,
                "reason": handoff.reason,
                "checkpoint": checkpoint,
                "pack": pack,
            }),
        );
        event.blob = Some(transcript_hash);
        tx.append_event(event).await?;
        move_step(&mut tx, &mut current, StepStatus::Dispatched, None).await?;
        let prompt = self.agent_prompt(&mut tx, &current).await?;
        let mut effects = Vec::new();
        let from_session = current.session_id;
        // Same runtime: resume the CLI's own session first —
        // except a context rotation, whose point is a fresh session.
        let native_id = match from_session {
            Some(id)
                if handoff.to == handoff.from
                    && handoff.reason != HandoffReason::ContextRotation =>
            {
                tx.get_session(id).await?.native_id
            }
            _ => None,
        };
        let resume = native_id.map(|native_id| Resume {
            native_id,
            runtime: handoff.to.clone(),
            prompt: prompt.clone(),
        });
        let first = match &resume {
            Some(_) => format!(
                "Sun resumed this session after it stopped ({}). Continue the step \
                 where you left off; `{}` says what happened meanwhile.\n\n{prompt}",
                handoff.reason.as_str(),
                sun_context::HANDOFF_PATH
            ),
            None => prompt,
        };
        // A marketplace move: the run lives on the seller's host from now on
        // (its worktree, sessions, gates and pushes), running the leased adapter.
        let mut to = handoff.to.clone();
        if let Some((host, adapter)) = seller {
            let key = crate::queue::moved_key(current.run_id);
            tx.put_setting(&key, &json!(host)).await?;
            self.placed.insert(current.run_id, host);
            to = adapter;
        }
        // The pack is P1 of the new session's context (`.sun/context/handoff.md`).
        let to_session = self
            .assign(
                &mut tx,
                &current,
                &to,
                &first,
                Some(pack),
                resume,
                &mut effects,
            )
            .await?;
        if let Some(from_session) = from_session {
            tx.insert_handoff_pack(&model::HandoffPack {
                id: HandoffPackId::generate(),
                run_id: current.run_id,
                from_session,
                to_session: Some(to_session),
                pack_hash,
            })
            .await?;
        }
        tx.commit().await?;
        Box::pin(self.apply(effects)).await
    }

    /// Commits uncommitted work as `wip(sun): handoff from <runtime>` (the
    /// agent's hooks skipped, nothing pushed) and reads the git state.
    /// `Ok(None)`: the worktree is not a git repo. A stored secret
    /// in the change blocks the commit (`InvalidInput`).
    async fn checkpoint(
        &self,
        run_id: RunId,
        worktree: &std::path::Path,
        from: &str,
        scan: &SecretScan,
    ) -> Result<Option<GitState>> {
        let message = crate::gates::quote(&format!("wip(sun): handoff from {from}"));
        let command = format!(
            "git rev-parse --is-inside-work-tree >/dev/null 2>&1 || exit 3\n\
             git add -A || exit 1\n\
             {scan}\
             if [ -n \"$(git status --porcelain)\" ]; then\n\
               git -c user.name=sun -c user.email=sun@localhost \
             -c core.hooksPath=/dev/null commit -q --no-verify -m {message} || exit 1\n\
               echo checkpoint\n\
             else echo clean; fi\n\
             git rev-parse --abbrev-ref HEAD\n\
             git show --stat --format= HEAD | tail -n 1\n\
             git log --format='%h %s' -n 5\n",
            scan = scan.script(),
        );
        let out = self
            .hosts
            .exec(
                self.host(run_id),
                ExecRequest {
                    cwd: worktree.to_path_buf(),
                    command,
                    env: scan.env().collect(),
                    timeout_s: 60,
                },
            )
            .await?;
        match out.code {
            Some(0) => {}
            Some(3) => return Ok(None),
            Some(SECRET_FOUND) => return Err(Error::InvalidInput(out.output.trim().to_owned())),
            _ => {
                return Err(Error::Unavailable(format!(
                    "checkpoint commit failed: {}",
                    out.output.trim()
                )));
            }
        }
        let mut lines = out.output.lines().map(str::trim);
        let committed = lines.next() == Some("checkpoint");
        Ok(Some(GitState {
            branch: lines.next().unwrap_or_default().to_owned(),
            diff_stat: lines.next().unwrap_or_default().to_owned(),
            commits: lines.filter(|l| !l.is_empty()).map(str::to_owned).collect(),
            // The head commit holds work the agent had not committed.
            uncommitted: committed,
        }))
    }

    /// Where a hand-off to `market:<adapter>` stands, by the task's lease.
    async fn market_move(&mut self, step: &Step, handoff: &Handoff) -> Result<MarketMove> {
        let mut read = self.store.read().await?;
        let task = read.get_run(step.run_id).await?.task_id;
        let lease = crate::market::task_lease(&mut read, task).await?;
        drop(read);
        Ok(match lease {
            Some(TaskLease {
                lease_id: Some(_),
                host_id: Some(host),
                ..
            }) => MarketMove::To(host),
            // `bind_market_host` (or `release_lease_hold`) wakes the step.
            Some(_) => MarketMove::Wait,
            // Refused (the hold was released): back to `from` at its reset.
            None => match self.wait_for_reset(step, &handoff.from).await? {
                true => MarketMove::Wait,
                false => MarketMove::Back,
            },
        })
    }

    /// While `runtime` cools down, arms the step's timer for its reset (the
    /// hand-off is redone then) and returns true.
    async fn wait_for_reset(&mut self, step: &Step, runtime: &str) -> Result<bool> {
        let Some(until) = self.failover.cooling_until(runtime, Utc::now()) else {
            return Ok(false);
        };
        let timer = Effect::Timer {
            step_id: step.id,
            after: after(until),
        };
        Box::pin(self.apply(vec![timer])).await?;
        Ok(true)
    }

    /// Pushes the task branch from the run's host with the
    /// repo's git host token, as before a PR (`push`).
    async fn push_checkpoint(&self, run_id: RunId) -> std::result::Result<(), String> {
        let job = async {
            let mut read = self.store.read().await?;
            let facts = crate::outbox::RunFacts::load(&mut read, run_id).await?;
            let worktree = super::worktree_of(&mut read, facts.task.id).await?;
            let url = facts.repos.first().map_or("", |r| r.url.as_str());
            let mod_name = crate::memory::git_mod(url);
            let host = self.host(run_id);
            crate::push::Job::new(&mut read, host, &facts, worktree, mod_name, &self.vcs).await
        };
        match job.await.map_err(|e| e.to_string())? {
            Some(job) => job.run(self.hosts.as_ref()).await.map(drop),
            None => Err("the task has no repo to push".into()),
        }
    }

    /// `.sun/progress.md` of the worktree, or empty.
    async fn read_progress(&self, run_id: RunId, worktree: &std::path::Path) -> String {
        let request = ExecRequest {
            cwd: worktree.to_path_buf(),
            command: format!("cat {}", sun_context::PROGRESS_PATH),
            env: Default::default(),
            timeout_s: 10,
        };
        match self.hosts.exec(self.host(run_id), request).await {
            Ok(out) if out.code == Some(0) => out.output,
            _ => String::new(),
        }
    }
}

/// The step's agent events and the run's latest gate results, from the journal.
async fn journal_of(tx: &mut Tx, step: &Step) -> Result<(Vec<SunEvent>, Vec<GateResult>)> {
    let (mut events, mut gates) = (Vec::new(), HashMap::new());
    let mut cursor = 0;
    loop {
        let page = tx.events_after(Some(step.run_id), cursor, 500).await?;
        let Some(last) = page.last() else { break };
        cursor = last.seq;
        for event in page {
            if event.kind == "gate_result" {
                if let Ok(gate) =
                    serde_json::from_value::<GateResult>(event.payload["result"].clone())
                {
                    gates.insert(gate.name.clone(), gate);
                }
            } else if event.step_key.as_ref() == Some(&step.step_key)
                && let Ok(agent) = serde_json::from_value::<SunEvent>(event.payload)
            {
                events.push(agent);
            }
        }
    }
    let mut gates: Vec<GateResult> = gates.into_values().collect();
    gates.sort_by(|a, b| a.name.cmp(&b.name));
    Ok((events, gates))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pick_skips_cooling_runtimes_in_chain_order() {
        let chain: Vec<String> = ["claude-code", "codex", "opencode"]
            .map(String::from)
            .into();
        let now = Utc::now();
        let ready = HashMap::new();
        let mut failover = Failover::default();
        assert_eq!(
            failover.pick(&chain, now, &ready).as_deref(),
            Some("claude-code")
        );
        let soon = now + chrono::Duration::minutes(5);
        failover.cooling.insert("claude-code".into(), soon);
        assert_eq!(failover.pick(&chain, now, &ready).as_deref(), Some("codex"));
        let unready = HashMap::from([("codex".to_owned(), "not logged in".to_owned())]);
        assert_eq!(
            failover.pick(&chain, now, &unready).as_deref(),
            Some("opencode")
        );
        failover
            .cooling
            .insert("codex".into(), now + chrono::Duration::minutes(9));
        failover
            .cooling
            .insert("opencode".into(), now + chrono::Duration::minutes(7));
        assert_eq!(failover.pick(&chain, now, &ready), None);
        assert_eq!(failover.first_reset(&chain, now), Some(soon));
        // A cooldown in the past is over: the preferred runtime comes back.
        failover
            .cooling
            .insert("claude-code".into(), now - chrono::Duration::seconds(1));
        assert_eq!(
            failover.pick(&chain, now, &ready).as_deref(),
            Some("claude-code")
        );
    }

    #[test]
    fn failover_readiness_needs_the_adapter_installed_and_logged_in() {
        let capabilities: Capabilities = serde_json::from_value(json!({
            "os": "macos", "arch": "aarch64", "cpus": 8,
            "runtimes": [
                {"adapter": "claude-code", "readiness":
                    {"installed": true, "version": "2.1.3", "logged_in": true, "detail": null}},
                {"adapter": "codex", "readiness":
                    {"installed": true, "version": null, "logged_in": false, "detail": "run `codex login`"}},
            ],
        }))
        .unwrap();
        assert_eq!(unready_reason(&capabilities, "claude-code"), None);
        assert_eq!(
            unready_reason(&capabilities, "codex").as_deref(),
            Some("`codex` is not logged in: run `codex login`")
        );
        assert_eq!(
            unready_reason(&capabilities, "opencode").as_deref(),
            Some("`opencode` is not on the host")
        );
    }
}
