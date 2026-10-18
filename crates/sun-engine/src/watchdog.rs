//! The watchdog: stall, loop and budget detection, plus the
//! explicit-end rules, all driven by the agent event stream.
//!
//! **Pattern:** an **Observer** of each running agent step's events, with a
//! **Strategy** ([`StallPolicy`]) for stalls. [`Watchdog`] holds the pure
//! detection state; the `impl Executor` block enforces its verdicts inside the
//! executor's transaction. This file is a child module of `executor` (see the
//! `#[path]` there), so it reuses the executor's private helpers instead of
//! widening their visibility.
//!
//! What it watches, per agent step while it is `running`:
//! - **Stall:** no `ToolCall`, `FileChanged` or `Text` for
//!   [`WatchdogConfig::stall_after`] → `stalled`, then the [`StallPolicy`].
//! - **Loop:** the same `(tool, args_hash)`, or the same error, K times in the
//!   last `window` calls / errors → `looping`.
//! - **Budget** (per run, [`Budget`]): wall time since the run started,
//!   assistant turns, and cost from `Usage` → `budget_exceeded`. The cost
//!   cap is the smaller of the workflow's and the task's (`budget`).
//! - **Ecosystem cap** (`budget`): a `Usage` cost that takes the month's
//!   spend to the cap → `budget_exceeded`.
//! - **Explicit end:** a done claim with no output (no assistant text, no file
//!   change) and an exit without a done claim both fail the step.
//!
//! `looping` and `budget_exceeded` are terminal: no nudge and no hand-off
//! retry. A fresh session runs into the same run budget, and a looping agent
//! tends to repeat the loop; a human re-queues the task instead.
//!
//! Stall and wall checks are deadlines: one timer per step, armed when the step
//! starts running. When it fires early (there was activity) it is re-armed for
//! the rest. Nothing polls the agent.

use std::collections::{HashMap, VecDeque};
use std::time::Duration;

use chrono::Utc;
use sun_core::model::{Budget, Run, Step};
use sun_core::{
    Error, Result, Role, RunId, SessionHandle, Steer, StepId, StepStatus, SunEvent, Timestamp,
};
use sun_store::{EventLog, RunRepo, Tx};
use sun_workflow::{StepKind, WatchdogPolicy};
use tokio::time::Instant;

use serde_json::json;
use sun_workflow::QuestionKind;

use super::{Command, Effect, Executor, Hook, Settled, decision, kind, move_step};

/// Sent to a stalled agent under [`StallPolicy::Nudge`].
const NUDGE: &str = "Sun watchdog: no progress was seen for a while. Say in one line what \
                     you are doing, then continue, or call sun.done if the work is finished.";

/// Appended to the step prompt when [`StallPolicy::Restart`] starts a new session.
const HANDOFF: &str = "Hand-off from Sun: a previous session on this step stalled and was \
                       stopped. Its changes are in this worktree. Check them, then continue \
                       the work from there.";

pub use sun_workflow::StallPolicy;

/// Watchdog settings (defaults, `sun.yaml`). A workflow's
/// `watchdog:` block overrides `stall_after` and `on_stall` for its runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WatchdogConfig {
    /// No activity for this long marks the step `stalled`. Default 15 minutes.
    pub stall_after: Duration,
    /// What to do then.
    pub on_stall: StallPolicy,
    /// K: this many repeats of one tool call or error mean `looping`. Default 3.
    pub repeat_limit: usize,
    /// The sliding window: repeats count among the last this many tool calls
    /// (and, separately, errors). Default 10.
    pub window: usize,
    /// First wait before an agent step whose host is unreachable (start
    /// failed, or no probe answer after a restart) is tried again; it doubles
    /// per try, up to 60 s. Default 1 s.
    pub dispatch_backoff: Duration,
    /// After this many such tries the step fails and the run ends with
    /// "host … unreachable". Default 15 (about 10 minutes).
    pub dispatch_tries: u32,
    /// Context window in tokens per adapter id, from
    /// `RuntimeAdapter::context_window`. A runtime missing here is
    /// never rotated, and its packs assume a 200k window.
    pub context_windows: HashMap<String, u32>,
    /// A session whose `Usage.context_used` reaches this percent of its window
    /// is rotated to a fresh one at the next tool result. Default 80.
    pub rotate_at_percent: u8,
}

impl Default for WatchdogConfig {
    fn default() -> Self {
        Self {
            stall_after: Duration::from_secs(15 * 60),
            on_stall: StallPolicy::Nudge,
            repeat_limit: 3,
            window: 10,
            dispatch_backoff: Duration::from_secs(1),
            dispatch_tries: 15,
            context_windows: HashMap::new(),
            rotate_at_percent: 80,
        }
    }
}

/// Why the watchdog ends a step.
#[derive(Debug, Clone, PartialEq)]
pub(super) enum Verdict {
    Looping(String),
    BudgetExceeded(String),
    /// Ends through `verifying → failed` (empty output, exit without done).
    Failed(String),
}

/// Result of a deadline check.
#[derive(Debug, PartialEq)]
enum Check {
    /// Nothing yet; check again after this long.
    Due(Duration),
    Stalled,
    WallExceeded(String),
}

/// Detection state. Steps are forgotten when they stop running; the run
/// meters are rebuilt from the journal after a restart.
pub(super) struct Watchdog {
    config: WatchdogConfig,
    steps: HashMap<StepId, Watch>,
    runs: HashMap<RunId, Meter>,
    /// The ecosystem's spend this month, `(month start, usd)`, while a cap is
    /// set: seeded from the journal, then counted from each `Usage`.
    // ponytail: cost reported after a step stopped running isn't added until
    // the next reseed (month change, or the cap cleared and set again).
    spent: Option<(Timestamp, f64)>,
}

/// One running agent step attempt.
struct Watch {
    run_id: RunId,
    last_active: Instant,
    calls: VecDeque<(String, u64)>,
    errors: VecDeque<String>,
    /// The latest agent activity, one line each, for the decision engine.
    recent: VecDeque<String>,
    /// Any assistant text or file change so far.
    output: bool,
    /// The stall policy already acted once.
    escalated: bool,
    /// A deadline timer is pending.
    timer: bool,
}

/// What a run has spent so far, and its stall settings.
struct Meter {
    budget: Budget,
    /// `stall_after` for this run (workflow, else engine-wide).
    stall_after: Duration,
    /// `on_stall` for this run (workflow, else engine-wide).
    on_stall: StallPolicy,
    started_at: Timestamp,
    turns: u32,
    cost_usd: f64,
}

impl Meter {
    /// Counts turns and cost; returns why the budget is blown, if it is.
    fn count(&mut self, event: &SunEvent) -> Option<String> {
        match event {
            SunEvent::Text {
                role: Role::Assistant,
                ..
            } => self.turns += 1,
            SunEvent::Usage {
                cost_usd: Some(cost),
                ..
            } => self.cost_usd += cost,
            _ => return None,
        }
        if let Some(max) = self.budget.turns
            && self.turns > max
        {
            return Some(format!("turn budget of {max} exceeded"));
        }
        if let Some(max) = self.budget.cost_usd
            && self.cost_usd > max
        {
            return Some(format!(
                "cost ${:.2} exceeds the ${max:.2} budget",
                self.cost_usd
            ));
        }
        None
    }

    /// Wall time left, or why it ran out. `None`: no wall budget.
    fn wall_left(&self) -> Option<std::result::Result<Duration, String>> {
        let secs = self.budget.wall_secs?;
        let end = self.started_at + chrono::Duration::seconds(i64::try_from(secs).ok()?);
        Some(match (end - Utc::now()).to_std() {
            Ok(left) if !left.is_zero() => Ok(left),
            _ => Err(format!("wall time budget of {secs}s exceeded")),
        })
    }
}

impl Watchdog {
    pub(super) fn new(config: WatchdogConfig) -> Self {
        Self {
            config,
            steps: HashMap::new(),
            runs: HashMap::new(),
            spent: None,
        }
    }

    /// Starts (or, for a new session, resets) watching `step`. Returns true
    /// when the caller must arm a timer.
    fn arm(&mut self, step: &Step) -> bool {
        let watch = self.steps.entry(step.id).or_insert_with(|| Watch {
            run_id: step.run_id,
            last_active: Instant::now(),
            calls: VecDeque::new(),
            errors: VecDeque::new(),
            recent: VecDeque::new(),
            output: false,
            escalated: false,
            timer: false,
        });
        watch.last_active = Instant::now();
        watch.calls.clear();
        watch.errors.clear();
        !watch.timer
    }

    fn forget(&mut self, step_id: StepId) {
        if let Some(watch) = self.steps.remove(&step_id)
            && !self.steps.values().any(|w| w.run_id == watch.run_id)
        {
            self.runs.remove(&watch.run_id);
        }
    }

    /// Agent output that reached the kernel outside the session's stream: a
    /// `sun.done` summary over MCP can overtake the stream's own events.
    pub(super) fn saw_output(&mut self, step_id: StepId) {
        if let Some(watch) = self.steps.get_mut(&step_id) {
            watch.output = true;
        }
    }

    /// Feeds one agent event of a `running` step; returns a verdict that ends it.
    pub(super) fn observe(&mut self, step_id: StepId, event: &SunEvent) -> Option<Verdict> {
        let (limit, window) = (self.config.repeat_limit, self.config.window);
        let watch = self.steps.get_mut(&step_id)?;
        if let Some(line) = activity(event) {
            if watch.recent.len() == RECENT {
                watch.recent.pop_front();
            }
            watch.recent.push_back(line);
        }
        if matches!(
            event,
            SunEvent::ToolCall { .. } | SunEvent::FileChanged { .. } | SunEvent::Text { .. }
        ) {
            watch.last_active = Instant::now();
        }
        match event {
            SunEvent::Text {
                role: Role::Assistant,
                text,
            } if !text.trim().is_empty() => watch.output = true,
            SunEvent::FileChanged { .. } => watch.output = true,
            SunEvent::ToolCall {
                name, args_hash, ..
            } => {
                if repeated(&mut watch.calls, (name.clone(), *args_hash), window, limit) {
                    return Some(Verdict::Looping(format!(
                        "`{name}` called {limit} times with the same arguments"
                    )));
                }
            }
            SunEvent::Error { message }
            | SunEvent::ToolResult {
                ok: false,
                summary: message,
            } => {
                if repeated(&mut watch.errors, message.clone(), window, limit) {
                    return Some(Verdict::Looping(format!(
                        "the same error {limit} times: {message}"
                    )));
                }
            }
            SunEvent::DoneClaimed { .. } if !watch.output => {
                return Some(Verdict::Failed(
                    "empty output: done claimed without any agent text or file change".into(),
                ));
            }
            SunEvent::Exited { code } => {
                let how = code.map_or_else(|| "by a signal".to_owned(), |c| format!("code {c}"));
                return Some(Verdict::Failed(format!(
                    "the agent exited ({how}) without claiming done"
                )));
            }
            _ => {}
        }
        let meter = self.runs.get_mut(&watch.run_id)?;
        meter.count(event).map(Verdict::BudgetExceeded)
    }

    /// The deadline check a timer runs.
    fn check(&self, step_id: StepId) -> Option<Check> {
        let watch = self.steps.get(&step_id)?;
        let meter = self.runs.get(&watch.run_id);
        let stall_after = meter.map_or(self.config.stall_after, |m| m.stall_after);
        let mut due = (watch.last_active + stall_after).saturating_duration_since(Instant::now());
        if let Some(wall) = meter.and_then(Meter::wall_left) {
            match wall {
                Ok(left) => due = due.min(left),
                Err(why) => return Some(Check::WallExceeded(why)),
            }
        }
        Some(if due.is_zero() {
            Check::Stalled
        } else {
            Check::Due(due)
        })
    }
}

/// Agent activity lines kept per step for the stall question.
const RECENT: usize = 20;
/// Longest activity line, in chars.
const MAX_LINE: usize = 200;

/// One line of agent activity, for the decision engine.
fn activity(event: &SunEvent) -> Option<String> {
    let line = match event {
        SunEvent::Text { role, text } => format!("{role:?}: {text}"),
        SunEvent::ToolCall { name, summary, .. } => format!("tool {name}: {summary}"),
        SunEvent::ToolResult { ok, summary } => {
            format!("result ({}): {summary}", if *ok { "ok" } else { "error" })
        }
        SunEvent::Error { message } => format!("error: {message}"),
        SunEvent::FileChanged { .. } => format!("{event:?}"),
        _ => return None,
    };
    Some(line.chars().take(MAX_LINE).collect())
}

/// Pushes `item` into the sliding window; true when it now holds `limit` copies.
fn repeated<T: PartialEq>(recent: &mut VecDeque<T>, item: T, window: usize, limit: usize) -> bool {
    recent.push_back(item);
    if recent.len() > window {
        recent.pop_front();
    }
    recent
        .back()
        .is_some_and(|last| recent.iter().filter(|x| *x == last).count() >= limit)
}

/// The run's meter, folded from its journal (wall start, turns, cost so far),
/// with the stall settings of `policy` over `config`.
// ponytail: one scan of the run's events per run per boot.
async fn meter_from_journal(
    tx: &mut Tx,
    run: &Run,
    config: &WatchdogConfig,
    policy: Option<WatchdogPolicy>,
) -> Result<Meter> {
    let policy = policy.unwrap_or_default();
    let mut budget = run.budget.clone();
    budget.cost_usd = crate::budget::tighter(
        budget.cost_usd,
        crate::budget::task_budget(tx, run.task_id).await?,
    );
    let mut meter = Meter {
        budget,
        stall_after: policy
            .stall_after_secs
            .map_or(config.stall_after, Duration::from_secs),
        on_stall: policy.on_stall.unwrap_or(config.on_stall),
        started_at: Utc::now(),
        turns: 0,
        cost_usd: 0.0,
    };
    let mut started = None;
    let mut after = 0;
    loop {
        let page = tx.events_after(Some(run.id), after, 500).await?;
        let Some(last) = page.last() else {
            break;
        };
        after = last.seq;
        for event in &page {
            match event.kind.as_str() {
                kind::RUN_STATUS if started.is_none() && event.payload["to"] == "running" => {
                    started = Some(event.at);
                }
                "text" | "usage" => {
                    if let Ok(agent) = serde_json::from_value::<SunEvent>(event.payload.clone()) {
                        meter.count(&agent);
                    }
                }
                _ => {}
            }
        }
    }
    meter.started_at = started.unwrap_or(meter.started_at);
    Ok(meter)
}

impl Executor {
    /// Hook: `step` just moved to `running` on a session. Starts its watch.
    pub(super) async fn watch(&mut self, tx: &mut Tx, step: &Step) -> Result<()> {
        if !self.watchdog.runs.contains_key(&step.run_id) {
            let run = tx.get_run(step.run_id).await?;
            let policy = self.workflow(tx, &run).await?.watchdog;
            let meter = meter_from_journal(tx, &run, &self.watchdog.config, policy).await?;
            self.watchdog.runs.insert(run.id, meter);
        }
        if self.watchdog.arm(step) {
            let after = match self.watchdog.check(step.id) {
                Some(Check::Due(after)) => after,
                _ => Duration::ZERO,
            };
            self.watch_timer(step.id, after);
        }
        Ok(())
    }

    /// Hook: an agent event of `step` was just stored. When the watchdog ends
    /// the step, this moves it, advances the run and returns true; the caller
    /// then skips its normal handling.
    pub(super) async fn police(
        &mut self,
        tx: &mut Tx,
        step: &mut Step,
        event: &SunEvent,
        effects: &mut Vec<Effect>,
    ) -> Result<bool> {
        if step.status != StepStatus::Running {
            return Ok(false);
        }
        let verdict = match self.watchdog.observe(step.id, event) {
            Some(verdict) => verdict,
            None => match event {
                SunEvent::Usage {
                    cost_usd: Some(cost),
                    ..
                } => match self.over_ecosystem_cap(tx, *cost).await? {
                    Some(why) => Verdict::BudgetExceeded(why),
                    None => return Ok(false),
                },
                _ => return Ok(false),
            },
        };
        self.enforce(tx, step, verdict, effects).await?;
        self.advance(tx, step.run_id, effects).await?;
        Ok(true)
    }

    /// Adds `cost` (already journalled in `tx`) to the month's spend; why
    /// the ecosystem cap is now reached, if it is.
    async fn over_ecosystem_cap(&mut self, tx: &mut Tx, cost: f64) -> Result<Option<String>> {
        let Some(cap) = crate::budget::monthly_cap(tx).await? else {
            self.watchdog.spent = None;
            return Ok(None);
        };
        let since = crate::budget::month_start(Utc::now());
        let spent = match self.watchdog.spent {
            Some((at, spent)) if at == since => spent + cost,
            _ => crate::budget::spent_since(tx, since).await?,
        };
        self.watchdog.spent = Some((since, spent));
        Ok(crate::budget::over_cap(cap, spent))
    }

    /// A step's deadline timer fired ([`Command::Watch`]).
    pub(super) async fn on_watch(&mut self, step_id: StepId) -> Result<()> {
        if let Some(watch) = self.watchdog.steps.get_mut(&step_id) {
            watch.timer = false;
        }
        let mut tx = self.store.begin().await?;
        let mut step = tx.get_step(step_id).await?;
        if step.status != StepStatus::Running {
            self.watchdog.forget(step_id);
            return Ok(());
        }
        let mut effects = Vec::new();
        let mut nudge = None;
        match self.watchdog.check(step_id) {
            None => return Ok(()),
            Some(Check::Due(after)) => {
                self.watch_timer(step_id, after);
                return Ok(());
            }
            Some(Check::WallExceeded(why)) => {
                let verdict = Verdict::BudgetExceeded(why);
                self.enforce(&mut tx, &mut step, verdict, &mut effects)
                    .await?;
            }
            // With a decision backend, it judges first: stuck,
            // looping or still working? The rule says stuck.
            Some(Check::Stalled) => match (self.decisions.get(), self.stall_hook(step_id)) {
                (Some(client), Some(hook)) => {
                    drop(tx);
                    let ask = Effect::Ask {
                        step_id,
                        hook,
                        client,
                    };
                    return self.apply(vec![ask]).await;
                }
                _ => nudge = self.on_stall(&mut tx, &mut step, &mut effects).await?,
            },
        }
        self.after_watch(tx, &step, nudge, effects).await
    }

    /// Advances the run, commits, nudges and applies the effects.
    async fn after_watch(
        &mut self,
        mut tx: Tx,
        step: &Step,
        nudge: Option<SessionHandle>,
        mut effects: Vec<Effect>,
    ) -> Result<()> {
        self.advance(&mut tx, step.run_id, &mut effects).await?;
        tx.commit().await?;
        if let Some(session) = nudge {
            // Best effort: if the steer is lost, the next stall fails the step.
            let text = NUDGE.to_owned();
            let _ = self.hosts.steer(&session, Steer { text }).await;
        }
        self.apply(effects).await
    }

    /// The stall question about a watched step's recent activity.
    fn stall_hook(&self, step_id: StepId) -> Option<Hook> {
        let watch = self.watchdog.steps.get(&step_id)?;
        let quiet = watch.last_active.elapsed().as_secs();
        Some(Hook::new(
            decision::WATCHDOG,
            QuestionKind::Choice,
            "An agent has shown no activity for a while. From its recent output, is it still \
             working, stuck, or looping?",
            json!({
                "working": "still making progress; the work just takes long",
                "stuck": "waiting on something or stopped making progress",
                "looping": "repeating the same actions without getting anywhere",
            }),
            json!({ "quiet_secs": quiet, "recent": watch.recent }),
            json!({ "choice": "stuck" }),
        ))
    }

    /// The decision engine judged a stall ([`Self::stall_hook`]): `working`
    /// re-arms the stall timer, `looping` ends the step `looping`, and
    /// anything else (the rule: `stuck`) runs the stall policy.
    // ponytail: each confident `working` buys one more `stall_after`; the
    // run's wall budget is the backstop.
    pub(super) async fn on_stall_judged(
        &mut self,
        step_id: StepId,
        settled: Settled,
    ) -> Result<()> {
        let mut tx = self.store.begin().await?;
        let mut step = tx.get_step(step_id).await?;
        if step.status != StepStatus::Running || !self.watchdog.steps.contains_key(&step_id) {
            return Ok(());
        }
        settled
            .journal(&mut tx, step.run_id, Some(&step.step_key))
            .await?;
        let mut effects = Vec::new();
        let mut nudge = None;
        // The agent may have got going again while the question was out.
        let due = match self.watchdog.check(step_id) {
            Some(Check::Due(after)) => Some(after),
            _ => None,
        };
        match settled.choice() {
            _ if due.is_some() => self.watch_timer(step_id, due.unwrap_or_default()),
            Some("working") => {
                if let Some(watch) = self.watchdog.steps.get_mut(&step_id) {
                    watch.last_active = Instant::now();
                }
                self.watch_timer(step_id, self.watchdog.config.stall_after);
            }
            Some("looping") => {
                let why = "the decision engine judged the agent to be looping".to_owned();
                self.enforce(&mut tx, &mut step, Verdict::Looping(why), &mut effects)
                    .await?;
            }
            _ => nudge = self.on_stall(&mut tx, &mut step, &mut effects).await?,
        }
        self.after_watch(tx, &step, nudge, effects).await
    }

    /// `running → stalled`, then the policy. Returns the session to nudge
    /// after commit, if any.
    async fn on_stall(
        &mut self,
        tx: &mut Tx,
        step: &mut Step,
        effects: &mut Vec<Effect>,
    ) -> Result<Option<SessionHandle>> {
        let (stall_after, mut policy) = self.watchdog.runs.get(&step.run_id).map_or(
            (
                self.watchdog.config.stall_after,
                self.watchdog.config.on_stall,
            ),
            |m| (m.stall_after, m.on_stall),
        );
        let why = format!("no agent activity for {stall_after:?}");
        if let Some(watch) = self.watchdog.steps.get_mut(&step.id) {
            if watch.escalated {
                policy = StallPolicy::Fail;
            }
            watch.escalated = true;
        }
        move_step(tx, step, StepStatus::Stalled, Some(&why)).await?;
        match policy {
            StallPolicy::Nudge => {
                move_step(tx, step, StepStatus::Running, Some("nudged")).await?;
                self.watch(tx, step).await?;
                Ok(self
                    .sessions
                    .values()
                    .find(|(id, _)| *id == step.id)
                    .map(|(_, handle)| handle.clone()))
            }
            StallPolicy::Restart => {
                self.stop_step_sessions(step.id, effects);
                move_step(tx, step, StepStatus::HandingOff, None).await?;
                move_step(tx, step, StepStatus::Dispatched, None).await?;
                let prompt = format!("{}\n\n{HANDOFF}", self.agent_prompt(tx, step).await?);
                let adapter = self.runtime_of(tx, step).await?;
                self.assign_session(tx, step, &adapter, &prompt, None, effects)
                    .await?;
                Ok(None)
            }
            StallPolicy::Fail => {
                self.stop_step_sessions(step.id, effects);
                self.watchdog.forget(step.id);
                Ok(None)
            }
        }
    }

    /// Ends `step` for `verdict` and stops its session.
    async fn enforce(
        &mut self,
        tx: &mut Tx,
        step: &mut Step,
        verdict: Verdict,
        effects: &mut Vec<Effect>,
    ) -> Result<()> {
        match verdict {
            Verdict::Looping(why) => move_step(tx, step, StepStatus::Looping, Some(&why)).await?,
            Verdict::BudgetExceeded(why) => {
                move_step(tx, step, StepStatus::BudgetExceeded, Some(&why)).await?;
            }
            Verdict::Failed(why) => self.finish(tx, step, Err(why)).await?,
        }
        self.stop_step_sessions(step.id, effects);
        self.watchdog.forget(step.id);
        Ok(())
    }

    /// The agent prompt of `step` from the run's pinned workflow.
    /// The agent step's prompt; an agent's system workflow adds its task
    /// (`agents::with_task`). The one place a session's prompt starts from.
    pub(super) async fn agent_prompt(&mut self, tx: &mut Tx, step: &Step) -> Result<String> {
        let run = tx.get_run(step.run_id).await?;
        let workflow = self.workflow(tx, &run).await?;
        let prompt = workflow
            .steps
            .iter()
            .find_map(|def| match &def.kind {
                StepKind::Agent { prompt, .. } if def.id == step.step_key => Some(prompt.clone()),
                _ => None,
            })
            .ok_or_else(|| Error::NotFound(format!("agent step `{}`", step.step_key)))?;
        crate::agents::with_task(tx, &run, prompt).await
    }

    fn watch_timer(&mut self, step_id: StepId, after: Duration) {
        if let Some(watch) = self.watchdog.steps.get_mut(&step_id) {
            watch.timer = true;
        }
        let commands = self.commands.clone();
        tokio::spawn(async move {
            tokio::time::sleep(after).await;
            let _ = commands.send(Command::Watch(step_id)).await;
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn watched(budget: Budget) -> (Watchdog, StepId) {
        let mut dog = Watchdog::new(WatchdogConfig::default());
        let step = Step {
            id: StepId::generate(),
            run_id: RunId::generate(),
            step_key: sun_core::StepKey::new("build").unwrap(),
            attempt: 1,
            status: StepStatus::Running,
            runtime_id: None,
            credential_id: None,
            session_id: None,
            started_at: None,
            ended_at: None,
        };
        let meter = Meter {
            budget,
            stall_after: WatchdogConfig::default().stall_after,
            on_stall: StallPolicy::Nudge,
            started_at: Utc::now(),
            turns: 0,
            cost_usd: 0.0,
        };
        dog.runs.insert(step.run_id, meter);
        assert!(dog.arm(&step));
        (dog, step.id)
    }

    fn call(hash: u64) -> SunEvent {
        SunEvent::ToolCall {
            name: "Bash".into(),
            args_hash: hash,
            summary: "cargo test".into(),
        }
    }

    fn said(text: &str) -> SunEvent {
        SunEvent::Text {
            role: Role::Assistant,
            text: text.into(),
        }
    }

    #[test]
    fn watchdog_loop_counts_repeats_in_a_sliding_window() {
        let (mut dog, step) = watched(Budget::default());
        // Interleaved other calls don't hide a loop.
        for event in [call(7), call(1), call(7), call(2)] {
            assert_eq!(dog.observe(step, &event), None);
        }
        assert!(matches!(
            dog.observe(step, &call(7)),
            Some(Verdict::Looping(why)) if why.contains("`Bash` called 3 times")
        ));

        // Repeats that fell out of the window (10) don't count.
        let (mut dog, step) = watched(Budget::default());
        dog.observe(step, &call(7));
        dog.observe(step, &call(7));
        for hash in 100..110 {
            assert_eq!(dog.observe(step, &call(hash)), None);
        }
        assert_eq!(dog.observe(step, &call(7)), None);

        let (mut dog, step) = watched(Budget::default());
        let failed = SunEvent::ToolResult {
            ok: false,
            summary: "E0308 mismatched types".into(),
        };
        let error = SunEvent::Error {
            message: "E0308 mismatched types".into(),
        };
        assert_eq!(dog.observe(step, &failed), None);
        assert_eq!(dog.observe(step, &error), None);
        assert!(matches!(
            dog.observe(step, &failed),
            Some(Verdict::Looping(why)) if why.contains("same error 3 times")
        ));
    }

    #[test]
    fn watchdog_budgets_turns_and_cost() {
        let turns = Budget {
            turns: Some(2),
            ..Budget::default()
        };
        let (mut dog, step) = watched(turns);
        assert_eq!(dog.observe(step, &said("one")), None);
        assert_eq!(dog.observe(step, &said("two")), None);
        assert_eq!(
            dog.observe(step, &said("three")),
            Some(Verdict::BudgetExceeded("turn budget of 2 exceeded".into()))
        );

        let cost = Budget {
            cost_usd: Some(1.0),
            ..Budget::default()
        };
        let (mut dog, step) = watched(cost);
        let usage = SunEvent::Usage {
            input: 10,
            output: 10,
            cost_usd: Some(0.6),
            context_used: None,
        };
        assert_eq!(dog.observe(step, &usage), None);
        assert_eq!(
            dog.observe(step, &usage),
            Some(Verdict::BudgetExceeded(
                "cost $1.20 exceeds the $1.00 budget".into()
            ))
        );

        let wall = Budget {
            wall_secs: Some(60),
            ..Budget::default()
        };
        let (mut dog, step) = watched(wall);
        let run = dog.steps[&step].run_id;
        dog.runs.get_mut(&run).unwrap().started_at = Utc::now() - chrono::Duration::seconds(61);
        assert_eq!(
            dog.check(step),
            Some(Check::WallExceeded(
                "wall time budget of 60s exceeded".into()
            ))
        );
    }

    #[test]
    fn watchdog_empty_output_and_silent_exit_fail() {
        let done = SunEvent::DoneClaimed {
            summary: "all done".into(),
        };
        let (mut dog, step) = watched(Budget::default());
        assert_eq!(dog.observe(step, &said("  ")), None);
        assert!(matches!(
            dog.observe(step, &done),
            Some(Verdict::Failed(why)) if why.starts_with("empty output")
        ));

        let (mut dog, step) = watched(Budget::default());
        dog.observe(step, &said("fixed it"));
        assert_eq!(dog.observe(step, &done), None);

        let (mut dog, step) = watched(Budget::default());
        dog.observe(step, &said("working"));
        assert_eq!(
            dog.observe(step, &SunEvent::Exited { code: Some(0) }),
            Some(Verdict::Failed(
                "the agent exited (code 0) without claiming done".into()
            ))
        );
    }

    #[test]
    fn watchdog_stall_deadline_follows_activity() {
        let (mut dog, step) = watched(Budget::default());
        assert!(matches!(dog.check(step), Some(Check::Due(d)) if d > Duration::from_secs(890)));
        let run = dog.steps[&step].run_id;
        dog.runs.get_mut(&run).unwrap().stall_after = Duration::ZERO;
        assert_eq!(dog.check(step), Some(Check::Stalled));
    }
}
