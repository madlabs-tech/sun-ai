//! Getting an agent step onto its host.
//!
//! A child module of `executor`, like `failover`. Two rules:
//!
//! - **Resume by session id first.** A session that continues the same
//!   runtime's work (`wait_until_reset`, a lost session, see `failover`)
//!   starts with `SessionSpec::resume` = the old session's native id. If the
//!   CLI can't resume it, the start is refused, or the session ends (`error`
//!   and `exited`) before any other event. The engine then journals
//!   `resume_failed` (the notice) and starts a fresh session with the same
//!   prompt and hand-off pack.
//! - **A comment continues the task's last session.** The first session of
//!   a run a comment queued ([`Executor::follow_up`]) resumes the CLI
//!   session of the task's previous run when that one ran the same runtime
//!   on the same host (the CLI keeps its sessions on the host). Its fresh
//!   fallback reads the thread (`conversation::thread`).
//! - **Retry while the host is unreachable.** A failed
//!   `prepare_worktree`/`start_session` leaves the step `dispatched`; a
//!   session whose host didn't answer the probe after a restart stays
//!   `running`. Both are tried again after `dispatch_backoff · 2ⁿ` (at most
//!   60 s), so a host that (re)connects picks the work up. After
//!   `dispatch_tries` tries the step fails and the run ends with
//!   "host `<name>` unreachable after N tries: …". A start the host
//!   refuses by its own rule (`forbidden`: bypass on a bare host) fails
//!   at once: "host `<name>` refused the session: …".
//!
//! **Pattern:** retry with exponential backoff over the step **state
//! machine**; nothing here holds a transaction across host I/O.

use std::collections::HashMap;
use std::time::Duration;

use serde_json::json;
use sun_core::model::{Run, Step};
use sun_core::{Error, Result, RunStatus, SessionHandle, SessionId, StepId, StepStatus, SunEvent};
use sun_store::{HostRepo, RunRepo, RuntimeRepo, SessionRepo, TaskRepo, Tx};

use super::{Command, Effect, Executor, Start, append, kind, move_step};
use crate::{conversation, queue, queued, state};

/// A resumed session failed; payload `{step_id, session_id, native_id, error}`.
const RESUME_FAILED: &str = "resume_failed";
/// The longest wait between two tries.
const MAX_BACKOFF: Duration = Duration::from_secs(60);

/// Continue the CLI's own session `native_id` instead of a new one.
#[derive(Debug, Clone)]
pub(super) struct Resume {
    pub native_id: String,
    /// The runtime (chain name) a fresh fallback session starts on.
    pub runtime: String,
    /// The fallback session's prompt (without the "resumed" notice).
    pub prompt: String,
}

/// A started resume that has not shown any agent work yet.
struct Resuming {
    resume: Resume,
    handoff: Option<Box<sun_context::HandoffPack>>,
    /// The last `error` it reported.
    error: Option<String>,
}

/// In-memory retry state; a restart starts every count again.
#[derive(Default)]
pub(super) struct Dispatch {
    resuming: HashMap<SessionId, Resuming>,
    /// Tries so far per step whose host is unreachable.
    tries: HashMap<StepId, u32>,
    /// Failed starts waiting for their next try.
    starts: HashMap<StepId, Start>,
}

impl Dispatch {
    /// Watches the events of resumed sessions: `Some` when the resume failed
    /// (`exited` before anything but `error`).
    fn observe(&mut self, session: SessionId, event: &SunEvent) -> Option<Resuming> {
        match event {
            SunEvent::Error { message } => {
                if let Some(resuming) = self.resuming.get_mut(&session) {
                    resuming.error = Some(message.clone());
                }
                None
            }
            SunEvent::Exited { .. } => self.resuming.remove(&session),
            _ => {
                self.resuming.remove(&session);
                None
            }
        }
    }
}

/// `base · 2^(try-1)`, at most [`MAX_BACKOFF`].
fn backoff(base: Duration, attempt: u32) -> Duration {
    base.saturating_mul(1 << attempt.saturating_sub(1).min(16))
        .min(MAX_BACKOFF)
}

impl Executor {
    /// The session of `start` is up: forget the step's tries and watch a
    /// resume until it shows work.
    pub(super) fn started(&mut self, start: &Start, handle: &SessionHandle) {
        self.dispatch.tries.remove(&start.step.id);
        if let Some(resume) = &start.resume {
            self.dispatch.resuming.insert(
                handle.session_id,
                Resuming {
                    resume: resume.clone(),
                    handoff: start.handoff.clone(),
                    error: None,
                },
            );
        }
    }

    /// Hook in `on_host_event`, for a fresh event of a `running` step: true
    /// when it ended a failed resume, which then falls back to a fresh
    /// session (the caller skips its normal handling).
    pub(super) async fn resume_failed(
        &mut self,
        tx: &mut Tx,
        step: &mut Step,
        session: SessionId,
        event: &SunEvent,
        effects: &mut Vec<Effect>,
    ) -> Result<bool> {
        let Some(failed) = self.dispatch.observe(session, event) else {
            return Ok(false);
        };
        if step.status != StepStatus::Running {
            return Ok(false);
        }
        let error = failed.error.unwrap_or_else(|| "exited".into());
        self.notice(tx, step, session, &failed.resume, &error)
            .await?;
        self.stop_step_sessions(step.id, effects);
        let why = "resume failed: starting a fresh session";
        move_step(tx, step, StepStatus::HandingOff, Some(why)).await?;
        move_step(tx, step, StepStatus::Dispatched, None).await?;
        let Resume {
            runtime, prompt, ..
        } = failed.resume;
        let handoff = failed.handoff.map(|h| *h);
        self.assign_session(tx, step, &runtime, &prompt, handoff, effects)
            .await?;
        Ok(true)
    }

    async fn notice(
        &self,
        tx: &mut Tx,
        step: &Step,
        session: SessionId,
        resume: &Resume,
        error: &str,
    ) -> Result<()> {
        let payload = json!({
            "step_id": step.id,
            "session_id": session,
            "native_id": resume.native_id,
            "error": error,
        });
        append(tx, step, RESUME_FAILED, payload).await
    }

    /// The prompt (and resume) of an agent step's first session. On a run
    /// a comment queued: `prompt` with the thread, or a resume of the task's
    /// last session with the messages left for the step, falling back to
    /// that. Any other start: `prompt` as it is.
    pub(super) async fn follow_up(
        &mut self,
        tx: &mut Tx,
        step: &Step,
        runtime: &str,
        prompt: String,
    ) -> Result<(String, Option<Resume>)> {
        if step.attempt != 1 || !conversation::woken_by_comment(tx, step.run_id).await? {
            return Ok((prompt, None));
        }
        let run = tx.get_run(step.run_id).await?;
        let me = tx.get_task(run.task_id).await?.assignee_agent_id;
        let on = conversation::thread_of(tx, run.task_id).await?;
        let fresh = match conversation::thread(tx, on, me).await?.as_str() {
            "" => prompt,
            thread => format!("{prompt}\n\n{thread}"),
        };
        let native_id = match conversation::previous_run(tx, &run).await? {
            Some(previous) => self.last_session(tx, &previous, &run, runtime).await?,
            None => None,
        };
        let Some(native_id) = native_id else {
            return Ok((fresh, None));
        };
        let messages = queued::take(tx, step).await?;
        let first = format!("Sun resumed this session for new messages on your task.{messages}");
        let resume = Resume {
            native_id,
            runtime: runtime.to_owned(),
            prompt: fresh + &messages,
        };
        Ok((first, Some(resume)))
    }

    /// The CLI's own id of `previous`'s last agent session, when it ran
    /// `runtime` on the host `run` is on.
    async fn last_session(
        &self,
        tx: &mut Tx,
        previous: &Run,
        run: &Run,
        runtime: &str,
    ) -> Result<Option<String>> {
        let host = queue::placed_host(tx, previous.id).await?;
        if host.unwrap_or(self.config.host) != self.host(run.id) {
            return Ok(None);
        }
        let last = tx
            .steps_of_run(previous.id)
            .await?
            .into_iter()
            .filter(|s| s.session_id.is_some())
            .max_by_key(|s| s.started_at);
        let Some((Some(session), Some(runtime_id))) = last.map(|s| (s.session_id, s.runtime_id))
        else {
            return Ok(None);
        };
        if tx.get_runtime(runtime_id).await?.name != runtime {
            return Ok(None);
        }
        Ok(tx.get_session(session).await?.native_id)
    }

    /// `start_session` failed (the step is still `dispatched`, `tx` open).
    /// A refused resume starts fresh now; anything else tries again later,
    /// or gives up after the bound.
    pub(super) async fn start_failed(
        &mut self,
        tx: &mut Tx,
        step: &mut Step,
        mut start: Start,
        error: &Error,
        effects: &mut Vec<Effect>,
    ) -> Result<()> {
        if step.status != StepStatus::Dispatched {
            return Ok(());
        }
        // A host rule refused it (bypass on a bare host): no retry helps.
        if let Error::Forbidden(why) = error {
            return self.give_up(tx, step, why, true, effects).await;
        }
        // The host answered, so the CLI refused the resume: start fresh.
        if !matches!(error, Error::Unavailable(_))
            && let Some(resume) = start.resume.take()
        {
            self.notice(tx, step, start.session_id, &resume, &error.to_string())
                .await?;
            start.prompt = crate::gates::with_feedback(tx, step, &resume.prompt).await?;
            effects.push(Effect::Start(start));
            return Ok(());
        }
        if self.retry_later(step.id) {
            self.dispatch.starts.insert(step.id, start);
            Ok(())
        } else {
            self.give_up(tx, step, &error.to_string(), false, effects)
                .await
        }
    }

    /// Schedules [`Command::Retry`] for `step_id` after the backoff. False
    /// when the step is out of tries.
    pub(super) fn retry_later(&mut self, step_id: StepId) -> bool {
        let tries = self.dispatch.tries.entry(step_id).or_default();
        *tries += 1;
        if *tries > self.config.watchdog.dispatch_tries {
            return false;
        }
        let after = backoff(self.config.watchdog.dispatch_backoff, *tries);
        let commands = self.commands.clone();
        tokio::spawn(async move {
            tokio::time::sleep(after).await;
            let _ = commands.send(Command::Retry(step_id)).await;
        });
        true
    }

    /// [`Command::Retry`]: start a failed start again, or probe the session
    /// of a host that didn't answer.
    pub(super) async fn on_retry(&mut self, step_id: StepId) -> Result<()> {
        let step = self.store.read().await?.get_step(step_id).await?;
        let waiting = matches!(step.status, StepStatus::Dispatched | StepStatus::Running);
        if let Some(start) = self.dispatch.starts.remove(&step_id) {
            if step.status == StepStatus::Dispatched {
                return self.start_session(start).await;
            }
        } else if waiting && let Some(id) = step.session_id {
            let handle = self.store.read().await?.get_session(id).await?;
            let probe = self.hosts.is_alive(&handle).await;
            let mut tx = self.store.begin().await?;
            let mut step = tx.get_step(step_id).await?;
            let mut effects = Vec::new();
            if matches!(step.status, StepStatus::Dispatched | StepStatus::Running) {
                let prompt = self.agent_prompt(&mut tx, &step).await?;
                self.on_probe(&mut tx, &mut step, &prompt, handle, probe, &mut effects)
                    .await?;
            }
            tx.commit().await?;
            return self.apply(effects).await;
        }
        self.dispatch.tries.remove(&step_id);
        Ok(())
    }

    /// The bound is reached: the step fails and its run ends with the reason.
    async fn give_up(
        &mut self,
        tx: &mut Tx,
        step: &mut Step,
        error: &str,
        refused: bool,
        effects: &mut Vec<Effect>,
    ) -> Result<()> {
        let tries = self.config.watchdog.dispatch_tries;
        self.dispatch.tries.remove(&step.id);
        let host_id = self.host(step.run_id);
        let host = match tx.get_host(host_id).await {
            Ok(host) => host.name,
            Err(_) => host_id.to_string(),
        };
        let why = match refused {
            true => format!("host `{host}` refused the session: {error}"),
            false => {
                let why = format!("host `{host}` unreachable after {tries} tries: {error}");
                let payload = json!({ "step_id": step.id, "error": why });
                append(tx, step, kind::HOST_UNREACHABLE, payload).await?;
                why
            }
        };
        self.stop_step_sessions(step.id, effects);
        match step.status {
            StepStatus::Dispatched => move_step(tx, step, StepStatus::Failed, Some(&why)).await?,
            _ => self.finish(tx, step, Err(why.clone())).await?,
        }
        let mut latest = state::latest(tx.steps_of_run(step.run_id).await?);
        let run_id = step.run_id;
        self.end_run(
            tx,
            run_id,
            &mut latest,
            RunStatus::Failed,
            &why,
            true,
            effects,
        )
        .await
    }

    /// What a probe of the session of a `dispatched`/`running` agent step
    /// means (resume on boot, and [`Command::Retry`]): alive → re-attach;
    /// unreachable → keep routing its events and probe again later; gone →
    /// a lost `running` session is handed off to the same runtime (resume
    /// first), a `dispatched` one starts on a new session.
    pub(super) async fn on_probe(
        &mut self,
        tx: &mut Tx,
        step: &mut Step,
        prompt: &str,
        handle: SessionHandle,
        probe: Result<bool>,
        effects: &mut Vec<Effect>,
    ) -> Result<()> {
        // `NotFound`: no connected host runs it. When that includes the run's
        // own host (e.g. it restarted with the kernel), the session is gone.
        let probe = match probe {
            Err(Error::NotFound(_)) if self.hosts.is_online(self.host(step.run_id)) => Ok(false),
            probe => probe,
        };
        match probe {
            Ok(true) => {
                self.dispatch.tries.remove(&step.id);
                self.sessions
                    .insert(handle.session_id, (step.id, handle.clone()));
                if step.status == StepStatus::Dispatched {
                    move_step(tx, step, StepStatus::Running, None).await?;
                    append(
                        tx,
                        step,
                        kind::SESSION_STARTED,
                        json!({ "step_id": step.id, "session": handle }),
                    )
                    .await?;
                }
                self.watch(tx, step).await
            }
            // Host unreachable: the session may still run. Keep routing its
            // events; don't start a second one.
            Err(e) => {
                self.sessions.insert(handle.session_id, (step.id, handle));
                if self.retry_later(step.id) {
                    let payload = json!({ "step_id": step.id, "error": e.to_string() });
                    append(tx, step, kind::HOST_UNREACHABLE, payload).await
                } else {
                    self.give_up(tx, step, &e.to_string(), false, effects).await
                }
            }
            Ok(false) => {
                self.dispatch.tries.remove(&step.id);
                self.sessions.remove(&handle.session_id);
                if step.status == StepStatus::Running {
                    return self.lose_session(tx, step, effects).await;
                }
                let runtime = self.runtime_of(tx, step).await?;
                self.assign_session(tx, step, &runtime, prompt, None, effects)
                    .await
                    .map(drop)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn failover_dispatch_backoff_doubles_up_to_a_minute() {
        let base = Duration::from_secs(1);
        let waits: Vec<u64> = (1..=8).map(|n| backoff(base, n).as_secs()).collect();
        assert_eq!(waits, [1, 2, 4, 8, 16, 32, 60, 60]);
        assert_eq!(backoff(base, 200), MAX_BACKOFF);
    }

    #[test]
    fn failover_resume_fails_only_when_it_exits_before_any_work() {
        let (a, b) = (SessionId::generate(), SessionId::generate());
        let resuming = || Resuming {
            resume: Resume {
                native_id: "n".into(),
                runtime: "r".into(),
                prompt: "p".into(),
            },
            handoff: None,
            error: None,
        };
        let mut dispatch = Dispatch::default();
        dispatch.resuming.insert(a, resuming());
        dispatch.resuming.insert(b, resuming());
        let error = SunEvent::Error {
            message: "No conversation found".into(),
        };
        let exited = SunEvent::Exited { code: Some(1) };
        assert!(dispatch.observe(a, &error).is_none());
        let failed = dispatch.observe(a, &exited).unwrap();
        assert_eq!(failed.error.as_deref(), Some("No conversation found"));
        let text = SunEvent::Text {
            role: sun_core::Role::Assistant,
            text: "back".into(),
        };
        assert!(dispatch.observe(b, &text).is_none());
        assert!(dispatch.observe(b, &exited).is_none(), "it resumed");
    }
}
