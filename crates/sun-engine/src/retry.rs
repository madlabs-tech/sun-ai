//! Step retry: a failed step starts again from where it failed,
//! keeping every step that already succeeded. Child module of `executor`.
//!
//! - **New attempt.** A retry is a new row of the step key (`attempt + 1`).
//!   An agent step continues through the failover hand-off
//!   (`HandoffReason::Retry`): same worktree, the CLI's own session resumed
//!   when it can be, else a fresh session with a hand-off pack. Other steps
//!   queue again and `advance` starts them; an action's failed outbox rows
//!   go back to `pending` under the same idem key.
//! - **Policy.** A step's `retry: {max, backoff}` retries a `failed` attempt
//!   after `backoff · 2^(n-1)` while attempts are left; the run keeps going
//!   meanwhile ([`Executor::schedule_retries`]).
//! - **By hand.** [`Engine::retry_step`] retries now. A run that ended badly
//!   reopens (`failed → running`, its worktree was kept), and every other
//!   step the end cancelled or failed starts again with it.
//!
//! **Pattern:** **State machine** moves only; the retry edges live in
//! `sun_core` (`RunStatus::is_retryable`).

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use serde_json::json;
use sun_context::HandoffReason;
use sun_core::model::Step;
use sun_core::{Error, Result, RunId, RunStatus, StepId, StepKey, StepStatus};
use sun_store::{RunRepo, Tx};
use sun_workflow::{Step as StepDef, StepKind, Workflow};
use tokio::sync::oneshot;

use super::failover::{HANDOFF_STARTED, Handoff};
use super::{Command, Effect, Engine, Executor, append, kind, move_run, status_payload};
use crate::executor::Actor;
use crate::{queue, state};

/// A retry was asked for; payload `{step_id, attempt, by, acts_for}` (`by`:
/// the audit actor, or `retry_policy`; `acts_for`, a person's retry only:
/// whom the run acts for from here, see [`crate::bypass`]). `step_id` is the
/// new attempt.
pub(crate) const STEP_RETRY: &str = "step_retry";
/// A `retry:` policy will retry a failed attempt; `{step_id, attempt, after_secs}`.
const RETRY_SCHEDULED: &str = "retry_scheduled";

/// Who retries when a step's policy does.
pub(crate) const BY_POLICY: &str = "retry_policy";

/// `retry_step` replies.
pub(super) type Reply = oneshot::Sender<Result<()>>;

/// The latest attempt of a step ended in a way a retry can pick up.
fn retryable(status: StepStatus) -> bool {
    use StepStatus::*;
    matches!(
        status,
        Failed | Rejected | Cancelled | Stalled | Looping | BudgetExceeded
    )
}

impl Engine {
    /// Retries step `step_key` of `run_id` now, as `by`: a new
    /// attempt in the same worktree; succeeded steps never run again. A run
    /// that ended `failed`/`stalled`/`looping`/`budget_exceeded` reopens.
    /// `InvalidInput` when the step's latest attempt didn't end badly or
    /// the run can't reopen (succeeded, cancelled, or a newer run exists).
    /// From here the run acts for `by` (its bypass cap, [`crate::bypass`]).
    pub async fn retry_step(&self, run_id: RunId, step_key: StepKey, by: Actor) -> Result<()> {
        self.request(|reply| Command::RetryStep {
            run_id,
            step_key,
            by,
            reply,
        })
        .await
    }
}

impl Executor {
    pub(super) async fn retry_step(
        &mut self,
        run_id: RunId,
        step_key: StepKey,
        by: Actor,
    ) -> Result<()> {
        let mut tx = self.store.begin().await?;
        let run = tx.get_run(run_id).await?;
        let workflow = self.workflow(&mut tx, &run).await?;
        let latest = state::latest(tx.steps_of_run(run_id).await?);
        let step = latest
            .get(&step_key)
            .ok_or_else(|| Error::InvalidInput(format!("step `{step_key}` has not run yet")))?;
        if !retryable(step.status) {
            return Err(Error::InvalidInput(format!(
                "step `{step_key}` is {}; only a step that failed can be retried",
                step.status
            )));
        }
        let mut effects = Vec::new();
        let mut again = vec![step.clone()];
        match run.status {
            RunStatus::Running => {}
            status if status.is_retryable() => {
                self.reopen(&mut tx, &run, &step_key).await?;
                // The end cancelled (or failed) these too: they start again.
                again.extend(
                    latest
                        .values()
                        .filter(|s| s.step_key != step_key && retryable(s.status))
                        .cloned(),
                );
            }
            other => {
                return Err(Error::InvalidInput(format!(
                    "run {run_id} is {other}; only a running run or one that failed can retry a step"
                )));
            }
        }
        for previous in again {
            let def = definition(&workflow, &previous.step_key)?;
            self.new_attempt(&mut tx, def, &previous, Some(&by), &mut effects)
                .await?;
        }
        self.advance(&mut tx, run_id, &mut effects).await?;
        tx.commit().await?;
        self.apply(effects).await
    }

    /// `failed → running` (task `failed → queued → running`) on the host
    /// the run was placed on, where its worktree is.
    async fn reopen(
        &mut self,
        tx: &mut Tx,
        run: &sun_core::model::Run,
        key: &StepKey,
    ) -> Result<()> {
        let newest = tx.runs_of_task(run.task_id).await?.pop().map(|r| r.id);
        if newest != Some(run.id) {
            return Err(Error::InvalidInput(format!(
                "task {} has a newer run; retry the step there",
                run.task_id
            )));
        }
        let why = format!("step `{key}` retried");
        move_run(tx, run.id, RunStatus::Running, Some(&why)).await?;
        if let Some(host) = queue::placed_host(tx, run.id).await? {
            self.placed.insert(run.id, host);
        }
        Ok(())
    }

    /// Inserts attempt `previous.attempt + 1` of the step and journals
    /// `step_retry`, by `by` (`None`: the step's `retry:` policy). An agent attempt starts `interrupted` with a pending
    /// hand-off to its runtime (resumed by `resume` after a crash); any
    /// other starts `queued` for `advance`.
    async fn new_attempt(
        &mut self,
        tx: &mut Tx,
        def: &StepDef,
        previous: &Step,
        by: Option<&Actor>,
        effects: &mut Vec<Effect>,
    ) -> Result<()> {
        let agent = matches!(def.kind, StepKind::Agent { .. });
        let step = Step {
            id: StepId::generate(),
            run_id: previous.run_id,
            step_key: previous.step_key.clone(),
            attempt: previous.attempt + 1,
            status: match agent {
                true => StepStatus::Interrupted,
                false => StepStatus::Queued,
            },
            // The hand-off resumes this session and runtime when it can.
            runtime_id: previous.runtime_id.filter(|_| agent),
            session_id: previous.session_id.filter(|_| agent),
            started_at: None,
            ended_at: None,
        };
        tx.insert_step(&step).await?;
        let why = format!("retry of attempt {}", previous.attempt);
        let payload = status_payload(&step, None, Some(&why));
        append(tx, &step, kind::STEP_STATUS, payload).await?;
        let mut payload = json!({ "step_id": step.id, "attempt": step.attempt });
        payload["by"] = json!(by.map_or(BY_POLICY, |a| a.name.as_str()));
        if let Some(acts_for) = crate::bypass::by_of(by) {
            payload[crate::bypass::ACTS_FOR] = acts_for;
        }
        append(tx, &step, STEP_RETRY, payload).await?;
        if agent {
            let runtime = self.runtime_of(tx, &step).await?;
            let handoff = Handoff {
                step_id: step.id,
                from: runtime.clone(),
                to: runtime,
                reason: HandoffReason::Retry,
                reset_at: None,
            };
            append(tx, &step, HANDOFF_STARTED, json!(handoff)).await?;
            effects.push(Effect::Handoff(handoff));
        }
        Ok(())
    }

    /// Hook in `advance`: arms a timer for each `failed` step whose
    /// `retry:` policy has attempts left, and returns their keys (the run
    /// doesn't end on them).
    pub(super) async fn schedule_retries(
        &mut self,
        tx: &mut Tx,
        workflow: &Workflow,
        latest: &HashMap<StepKey, Step>,
        effects: &mut Vec<Effect>,
    ) -> Result<HashSet<StepKey>> {
        let mut keys = HashSet::new();
        for def in &workflow.steps {
            let (Some(policy), Some(step)) = (def.retry, latest.get(&def.id)) else {
                continue;
            };
            let Some(after_secs) = policy
                .delay_after(step.attempt)
                .filter(|_| step.status == StepStatus::Failed)
            else {
                continue;
            };
            keys.insert(def.id.clone());
            // ponytail: in memory; a restart arms (and journals) it again.
            if self.retry_timers.insert(step.id) {
                let payload = json!({
                    "step_id": step.id,
                    "attempt": step.attempt + 1,
                    "after_secs": after_secs,
                });
                append(tx, step, RETRY_SCHEDULED, payload).await?;
                effects.push(Effect::Timer {
                    step_id: step.id,
                    after: Duration::from_secs(after_secs),
                });
            }
        }
        Ok(keys)
    }

    /// A policy timer of failed attempt `step` fired: retry it unless a
    /// human did first or the run ended meanwhile.
    pub(super) async fn retry_due(
        &mut self,
        tx: &mut Tx,
        step: &Step,
        effects: &mut Vec<Effect>,
    ) -> Result<()> {
        if !self.retry_timers.remove(&step.id) {
            return Ok(());
        }
        let run = tx.get_run(step.run_id).await?;
        let latest = state::latest(tx.steps_of_run(run.id).await?);
        let still_latest = latest.get(&step.step_key).map(|s| s.id) == Some(step.id);
        if run.status != RunStatus::Running || !still_latest {
            return Ok(());
        }
        let workflow = self.workflow(tx, &run).await?;
        let def = definition(&workflow, &step.step_key)?;
        self.new_attempt(tx, def, step, None, effects).await?;
        self.advance(tx, run.id, effects).await
    }
}

fn definition<'a>(workflow: &'a Workflow, key: &StepKey) -> Result<&'a StepDef> {
    workflow
        .steps
        .iter()
        .find(|d| d.id == *key)
        .ok_or_else(|| Error::NotFound(format!("step `{key}` in the workflow")))
}
