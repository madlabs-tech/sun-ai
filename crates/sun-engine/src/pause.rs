//! A human pauses and resumes a run, and rejects an approval with a
//! reason. Child module of `executor`.
//!
//! - **Pause:** `running → paused`. Running agent sessions are killed and
//!   their steps go `running → interrupted` (the edits stay in the task
//!   install). Nothing new starts while the run is paused; approvals keep
//!   waiting and gate checks already out still report.
//! - **Resume:** `paused → running`. Interrupted agent steps start a new
//!   session on their runtime (`interrupted → dispatched`), `limit_hit` steps
//!   decide their failover again, then the run advances.

use std::time::Duration;

use serde_json::json;
use sun_core::model::Step;
use sun_core::{Result, RunId, RunStatus, SessionHandle, SessionId, StepKey, StepStatus, StopMode};
use sun_store::{EventLog, NewEvent, RunRepo, Tx};
use sun_workflow::StepKind;
use tokio::sync::oneshot;

use super::{Command, Effect, Engine, Executor, move_run, move_step, state};
use crate::approval::Principal;

/// Step reason of agent steps a pause interrupted.
pub(super) const PAUSED: &str = "paused by a human";
/// Start of the pause reason of a session a person killed.
const KILLED_BY: &str = "session killed by ";

/// True when a person paused the run (Pause, Stop, a move, a killed
/// session), from its pause reason; other pauses (a limit's
/// `pause_and_notify`, a raised flag) wait for a person to decide.
pub(super) fn by_a_person(reason: Option<&str>) -> bool {
    reason.is_some_and(|r| r == PAUSED || r.starts_with(KILLED_BY))
}
/// Event kind of a session an admin killed.
const KILLED: &str = "session_killed";

impl Engine {
    /// Pauses a running run: its agents stop, nothing new starts.
    /// `InvalidTransition` unless the run is `running`.
    pub async fn pause(&self, run_id: RunId) -> Result<()> {
        self.request(|reply| Command::Pause { run_id, reply }).await
    }

    /// Resumes a paused run (also one `on_limit: pause_and_notify` paused).
    /// `InvalidTransition` unless the run is `paused`.
    pub async fn resume(&self, run_id: RunId) -> Result<()> {
        self.request(|reply| Command::Resume { run_id, reply })
            .await
    }

    /// Kills a live agent session to free its host: the run is
    /// paused with `session killed by <by>: <reason>` (resume starts a new
    /// session) and a `session_killed {session_id, reason, by}` event is
    /// journaled. A session no run knows is just stopped on its host
    /// (`Ok(None)`).
    pub async fn kill_session(
        &self,
        session_id: SessionId,
        reason: String,
        by: String,
    ) -> Result<Option<RunId>> {
        self.request(|reply| Command::Kill {
            session_id,
            reason,
            by,
            reply,
        })
        .await
    }

    /// Rejects a `waiting_approval` step like [`Engine::decide`] with
    /// `approved: false`, and records `reason` on the step and its
    /// `approval` event.
    pub async fn reject(
        &self,
        run_id: RunId,
        step_key: StepKey,
        by: Principal,
        reason: String,
    ) -> Result<()> {
        self.request(|reply| Command::Decide {
            run_id,
            step_key,
            approved: false,
            by,
            reason: Some(reason),
            reply,
        })
        .await
    }
}

/// `pause` / `resume` replies.
pub(super) type Reply = oneshot::Sender<Result<()>>;

impl Executor {
    pub(super) async fn pause_run(&mut self, run_id: RunId) -> Result<()> {
        let mut tx = self.store.begin().await?;
        let mut effects = Vec::new();
        self.pause_in(&mut tx, run_id, PAUSED, &mut effects).await?;
        tx.commit().await?;
        self.apply(effects).await
    }

    /// `running → paused` with `reason`, in the caller's transaction.
    pub(super) async fn pause_in(
        &mut self,
        tx: &mut Tx,
        run_id: RunId,
        reason: &str,
        effects: &mut Vec<Effect>,
    ) -> Result<()> {
        move_run(tx, run_id, RunStatus::Paused, Some(reason)).await?;
        for (_, mut step) in state::latest(tx.steps_of_run(run_id).await?) {
            if step.status == StepStatus::Running && self.is_agent(tx, &step).await? {
                move_step(tx, &mut step, StepStatus::Interrupted, Some(reason)).await?;
                self.stop_step_sessions(step.id, effects);
            }
        }
        Ok(())
    }

    /// See [`Engine::kill_session`].
    pub(super) async fn kill_session(
        &mut self,
        session_id: SessionId,
        reason: &str,
        by: &str,
    ) -> Result<Option<RunId>> {
        let Some((step_id, handle)) = self.sessions.get(&session_id).cloned() else {
            let handle = SessionHandle {
                session_id,
                native_id: None,
                pid: None,
            };
            self.hosts.stop_session(&handle, StopMode::Kill).await?;
            return Ok(None);
        };
        let mut tx = self.store.begin().await?;
        let step = tx.get_step(step_id).await?;
        let why = format!("{KILLED_BY}{by}: {reason}");
        let killed = json!({ "session_id": handle.session_id, "reason": reason, "by": by });
        let event = NewEvent::new(step.run_id, Some(step.step_key.clone()), KILLED, killed);
        tx.append_event(event).await?;
        let mut effects = Vec::new();
        self.pause_in(&mut tx, step.run_id, &why, &mut effects)
            .await?;
        tx.commit().await?;
        self.apply(effects).await?;
        Ok(Some(step.run_id))
    }

    pub(super) async fn resume_run(&mut self, run_id: RunId) -> Result<()> {
        let mut tx = self.store.begin().await?;
        let mut effects = Vec::new();
        self.resume_in(&mut tx, run_id, &mut effects).await?;
        tx.commit().await?;
        self.apply(effects).await
    }

    /// `paused → running`, in the caller's transaction.
    pub(super) async fn resume_in(
        &mut self,
        tx: &mut Tx,
        run_id: RunId,
        effects: &mut Vec<Effect>,
    ) -> Result<()> {
        move_run(tx, run_id, RunStatus::Running, None).await?;
        for (_, mut step) in state::latest(tx.steps_of_run(run_id).await?) {
            match step.status {
                StepStatus::Interrupted | StepStatus::HandingOff => {
                    if let Some(handoff) = self.pending_handoff(tx, &step).await? {
                        effects.push(Effect::Handoff(handoff));
                    } else if step.status == StepStatus::Interrupted {
                        move_step(tx, &mut step, StepStatus::Dispatched, None).await?;
                        let adapter = self.runtime_of(tx, &step).await?;
                        let prompt = self.agent_prompt(tx, &step).await?;
                        self.assign_session(tx, &step, &adapter, &prompt, None, effects)
                            .await?;
                    }
                }
                StepStatus::LimitHit => effects.push(Effect::Timer {
                    step_id: step.id,
                    after: Duration::ZERO,
                }),
                _ => {}
            }
        }
        self.advance(tx, run_id, effects).await
    }

    async fn is_agent(&mut self, tx: &mut Tx, step: &Step) -> Result<bool> {
        let run = tx.get_run(step.run_id).await?;
        let workflow = self.workflow(tx, &run).await?;
        Ok(workflow
            .steps
            .iter()
            .any(|def| def.id == step.step_key && matches!(def.kind, StepKind::Agent { .. })))
    }
}
