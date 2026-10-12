//! Eval replay of past tasks: does a changed instruction,
//! memory or workflow version do better on work Sun already did?
//!
//! **Pick** past tasks with a finished run ([`Pick`]: a workflow, a
//! correction cause, a saved [`EvalSet`] or ids) and replay each one in a
//! **sandbox**: a throwaway store in a temp dir holding a copy of the task
//! (fresh id, so a fresh `sun/<id>` worktree), its workflow (the candidate
//! YAML, else the latest version), repos, hosts, pinned context, the active
//! memory plus the candidate's lines, and a second [`Engine`] on it:
//! - **Hosts** go through [`Sandbox`] (**Decorator** over `HostLink`). With
//!   no runtime named, every session replays the original step's recorded
//!   transcript in-kernel ([`REPLAY`]), so no model runs; a named runtime
//!   runs for real on the host. The push before an `open_pr` is skipped.
//! - **Mods** go through [`Recorder`]: outbox actions are recorded, never
//!   delivered.
//! - **Approvals** reuse the original run's recorded decisions in order
//!   per step (a human rejection, with its reason, recurs); one the original
//!   never reached is approved as its first `who:` user (or roles).
//!   [`EvalSpec::auto_approve`] approves every one instead.
//! - **Base:** each repo's worktree starts from the commit the original run
//!   was cut from (its `run_base`), not the base branch's current tip.
//!
//! The [`EvalReport`] puts each original run beside its replay: passed,
//! corrections by cause, and *repeats* — corrections whose cause the task
//! was already corrected for (in runs before the original, or earlier in the
//! same run) — with the pass-rate and repeat deltas.

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::Duration;

use async_trait::async_trait;
use chrono::Utc;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sun_core::host_protocol::{
    DeviceRequest, DeviceResult, ExecOutput, ExecRequest, SessionFile, SessionStats, Worktree,
};
use sun_core::model::{
    CorrectionCause, EvalRun, EvalSet, EvalStatus, EventRecord, MemoryItem, MemoryScope,
    MemoryStatus, Run, Task,
};
use sun_core::{
    BoxStream, Error, EvalRunId, EvalSetId, HostEvent, HostId, HostLink, MemoryItemId, ModCall,
    ModLink, Result, RunId, RunStatus, SessionHandle, SessionId, SessionSpec, StateMachine, Steer,
    StepKey, StepStatus, StopMode, SunEvent, TaskId, TaskStatus, WorkflowId,
};
use sun_store::{
    CorrectionRepo, EvalRepo, EventLog, HostRepo, MemoryRepo, NewEvent, RepoRepo, RunRepo,
    SettingRepo, Store, TaskRepo, Tx, WorkflowRepo,
};
use sun_workflow::{StepKind, Workflow};
use tokio::sync::mpsc;

use crate::executor::kind::APPROVAL;
use crate::{Engine, EngineConfig, Principal, WatchdogConfig};

/// The runtime of a replay without one named: each session replays the
/// original step's recorded transcript, no model runs.
pub const REPLAY: &str = "replay";
/// Most tasks one eval replays.
pub const MAX_TASKS: u32 = 50;
const DEFAULT_TASKS: u32 = 20;
/// Most candidate memory lines, and the longest, in chars.
const MAX_LINES: usize = 20;
const MAX_LINE: usize = 300;
const MAX_NAME: usize = 200;
/// Corrections scanned for a [`Pick::Cause`].
const CAUSE_SCAN: u32 = 5000;
/// A replay still going after this is cancelled.
const TASK_TIMEOUT: Duration = Duration::from_secs(30 * 60);
const TICK: Duration = Duration::from_millis(500);
/// Gap between replayed transcript events.
const EVENT_GAP: Duration = Duration::from_millis(5);

/// Which past tasks to replay; each needs a finished run.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "by", rename_all = "snake_case")]
pub enum Pick {
    /// Tasks of one workflow.
    Workflow {
        /// The workflow.
        workflow_id: WorkflowId,
    },
    /// Tasks corrected for one cause, most recent first.
    Cause {
        /// The cause.
        cause: CorrectionCause,
    },
    /// A saved eval set.
    Set {
        /// The set.
        set_id: EvalSetId,
    },
    /// These tasks.
    Tasks {
        /// The tasks.
        task_ids: Vec<TaskId>,
    },
}

/// What changed: replayed instead of what the original runs had.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Candidate {
    /// Workflow YAML replayed instead of each task's latest version (a new
    /// step prompt, gate or step).
    #[serde(default)]
    pub workflow: Option<String>,
    /// Instruction lines added as active ecosystem memory (P3 of every pack).
    #[serde(default)]
    pub memory: Vec<String>,
}

/// An eval to run.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EvalSpec {
    /// Name.
    pub name: String,
    /// The past tasks.
    pub pick: Pick,
    /// The change under test.
    #[serde(default)]
    pub candidate: Candidate,
    /// `None` replays the recorded transcripts ([`REPLAY`]); a runtime name
    /// (e.g. `claude-code`) runs the agents for real, on quota.
    #[serde(default)]
    pub runtime: Option<String>,
    /// Most tasks (1–[`MAX_TASKS`], default 20).
    #[serde(default)]
    pub limit: Option<u32>,
    /// Approve every approval instead of replaying the original run's
    /// recorded decisions (so a human rejection does not recur).
    #[serde(default)]
    pub auto_approve: bool,
}

/// How one run of a task went.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Outcome {
    /// The run (a sandbox run for a replay).
    pub run_id: RunId,
    /// How it ended.
    pub status: RunStatus,
    /// It succeeded.
    pub passed: bool,
    /// Why it ended, when not succeeded.
    pub end_reason: Option<String>,
    /// Its corrections' causes, oldest first.
    pub causes: Vec<CorrectionCause>,
    /// Corrections whose cause the task was already corrected for.
    pub repeats: u32,
    /// Outbox actions recorded instead of delivered (replays only), e.g.
    /// `github.open_pr`.
    #[serde(default)]
    pub actions: Vec<String>,
}

/// One task, original beside replay.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TaskResult {
    /// The task.
    pub task_id: TaskId,
    /// Its title.
    pub title: String,
    /// The task's last finished run.
    pub original: Outcome,
    /// The replay; `None` when it could not run (`error`).
    pub replay: Option<Outcome>,
    /// Why the replay could not run.
    pub error: Option<String>,
}

/// Totals of one side over the tasks that replayed.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Side {
    /// Runs that succeeded.
    pub passed: u32,
    /// `passed` / tasks replayed (0 when none).
    pub pass_rate: f64,
    /// Corrections.
    pub mistakes: u32,
    /// Corrections repeating a known cause.
    pub repeats: u32,
}

/// The side-by-side result of an eval.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct EvalReport {
    /// Runtime the replays ran on ([`REPLAY`] or a runtime name).
    pub runtime: String,
    /// Per task.
    pub tasks: Vec<TaskResult>,
    /// Tasks that replayed (both sides count only these).
    pub replayed: u32,
    /// The original runs.
    pub original: Side,
    /// The replays.
    pub replay: Side,
    /// `replay.pass_rate - original.pass_rate`.
    pub pass_delta: f64,
    /// `replay.repeats - original.repeats` (negative is better).
    pub repeat_delta: i64,
}

impl Engine {
    /// Saves past tasks as a named eval set. Every task must exist.
    pub async fn save_eval_set(&self, name: &str, task_ids: Vec<TaskId>) -> Result<EvalSet> {
        let name = name_of(name)?;
        if task_ids.is_empty() || task_ids.len() > MAX_TASKS as usize {
            return Err(Error::InvalidInput(format!(
                "an eval set holds 1–{MAX_TASKS} tasks"
            )));
        }
        let set = EvalSet {
            id: EvalSetId::generate(),
            name,
            task_ids,
            created_at: Utc::now(),
        };
        let mut tx = self.store.begin().await?;
        for id in &set.task_ids {
            tx.get_task(*id).await?;
        }
        tx.insert_eval_set(&set).await?;
        tx.commit().await?;
        Ok(set)
    }

    /// Saved eval sets, newest first.
    pub async fn eval_sets(&self) -> Result<Vec<EvalSet>> {
        Ok(self.store.read().await?.eval_sets().await?)
    }

    /// The latest `limit` evals, newest first.
    pub async fn eval_runs(&self, limit: u32) -> Result<Vec<EvalRun>> {
        Ok(self.store.read().await?.eval_runs(limit).await?)
    }

    /// One eval with its report once done.
    pub async fn eval_run(&self, id: EvalRunId) -> Result<EvalRun> {
        Ok(self.store.read().await?.get_eval_run(id).await?)
    }

    /// Up to `limit` tasks `pick` selects that have a finished run.
    pub async fn pick_tasks(&self, pick: &Pick, limit: u32) -> Result<Vec<TaskId>> {
        let mut read = self.store.read().await?;
        let ids: Vec<TaskId> = match pick {
            Pick::Workflow { workflow_id } => read
                .tasks()
                .await?
                .into_iter()
                .filter(|t| t.workflow_id == Some(*workflow_id))
                .map(|t| t.id)
                .collect(),
            Pick::Cause { cause } => {
                let mut seen = HashSet::new();
                read.corrections(CAUSE_SCAN)
                    .await?
                    .into_iter()
                    .filter(|c| c.cause_label == *cause && seen.insert(c.task_id))
                    .map(|c| c.task_id)
                    .collect()
            }
            Pick::Set { set_id } => read.get_eval_set(*set_id).await?.task_ids,
            Pick::Tasks { task_ids } => task_ids.clone(),
        };
        let mut out = Vec::new();
        for id in ids {
            if out.len() >= limit as usize {
                break;
            }
            if original_run(&mut read, id).await?.is_some() {
                out.push(id);
            }
        }
        Ok(out)
    }

    /// Starts an eval: records it `running` and replays its tasks one by one
    /// in the background; [`Self::eval_run`] has the report when done.
    pub async fn start_eval(&self, spec: EvalSpec) -> Result<EvalRun> {
        let name = name_of(&spec.name)?;
        validate(&spec)?;
        let limit = spec.limit.unwrap_or(DEFAULT_TASKS).clamp(1, MAX_TASKS);
        let tasks = self.pick_tasks(&spec.pick, limit).await?;
        if tasks.is_empty() {
            return Err(Error::InvalidInput(
                "no past task with a finished run matches".into(),
            ));
        }
        let run = EvalRun {
            id: EvalRunId::generate(),
            name,
            spec: serde_json::to_value(&spec).map_err(protocol)?,
            status: EvalStatus::Running,
            report: None,
            error: None,
            created_at: Utc::now(),
            finished_at: None,
        };
        let mut tx = self.store.begin().await?;
        tx.insert_eval_run(&run).await?;
        tx.commit().await?;
        let (engine, id) = (self.clone(), run.id);
        tokio::spawn(async move {
            let result = engine.replay_all(&spec, tasks).await;
            if let Err(e) = engine.finish_eval(id, result).await {
                eprintln!("sun: eval {id}: {e}");
            }
        });
        Ok(run)
    }

    /// Fails evals left `running` by a restart (their replays are gone).
    pub async fn fail_interrupted_evals(&self) -> Result<usize> {
        let running: Vec<EvalRunId> = self
            .eval_runs(u32::MAX)
            .await?
            .into_iter()
            .filter(|r| r.status == EvalStatus::Running)
            .map(|r| r.id)
            .collect();
        for id in &running {
            self.finish_eval(
                id.to_owned(),
                Err(Error::Unavailable("interrupted by a restart".into())),
            )
            .await?;
        }
        Ok(running.len())
    }

    /// The recorded transcript of each agent step of `run_id`: its first
    /// session's events up to `exited`, as fake-runtime scripts (one
    /// `SunEvent` per JSONL line).
    pub async fn transcript(&self, run_id: RunId) -> Result<BTreeMap<StepKey, String>> {
        let mut read = self.store.read().await?;
        let mut out = BTreeMap::new();
        for (key, events) in transcript_events(&run_events(&mut read, run_id).await?) {
            let lines: Vec<String> = events
                .iter()
                .map(serde_json::to_string)
                .collect::<std::result::Result<_, _>>()
                .map_err(protocol)?;
            out.insert(key, lines.join("\n"));
        }
        Ok(out)
    }

    async fn finish_eval(&self, id: EvalRunId, result: Result<EvalReport>) -> Result<()> {
        let (status, report, error) = match result {
            Ok(report) => (
                EvalStatus::Done,
                Some(serde_json::to_value(report).map_err(protocol)?),
                None,
            ),
            Err(e) => (EvalStatus::Failed, None, Some(e.to_string())),
        };
        let mut tx = self.store.begin().await?;
        tx.finish_eval_run(id, status, report.as_ref(), error.as_deref(), Utc::now())
            .await?;
        tx.commit().await?;
        Ok(())
    }

    async fn replay_all(&self, spec: &EvalSpec, tasks: Vec<TaskId>) -> Result<EvalReport> {
        let mut report = EvalReport {
            runtime: spec.runtime.clone().unwrap_or_else(|| REPLAY.into()),
            ..EvalReport::default()
        };
        // ponytail: one task at a time; run them in parallel if evals get big.
        for task_id in tasks {
            let mut read = self.store.read().await?;
            let task = read.get_task(task_id).await?;
            let Some((run, prior)) = original_run(&mut read, task_id).await? else {
                continue;
            };
            let causes: Vec<CorrectionCause> = read
                .corrections_of(task_id)
                .await?
                .into_iter()
                .filter(|c| c.run_id == run.id)
                .map(|c| c.cause_label)
                .collect();
            drop(read);
            let original = Outcome {
                run_id: run.id,
                status: run.status,
                passed: run.status == RunStatus::Succeeded,
                end_reason: run.end_reason.clone(),
                repeats: repeats(&causes, &prior),
                causes,
                actions: Vec::new(),
            };
            let (replay, error) = match self.replay_task(spec, &task, &run).await {
                Ok(mut replay) => {
                    replay.repeats = repeats(&replay.causes, &prior);
                    (Some(replay), None)
                }
                Err(e) => (None, Some(e.to_string())),
            };
            report.tasks.push(TaskResult {
                task_id,
                title: task.title,
                original,
                replay,
                error,
            });
        }
        let replayed: Vec<(&Outcome, &Outcome)> = report
            .tasks
            .iter()
            .filter_map(|t| Some((&t.original, t.replay.as_ref()?)))
            .collect();
        report.replayed = u32::try_from(replayed.len()).unwrap_or(u32::MAX);
        report.original = side(replayed.iter().map(|(o, _)| *o));
        report.replay = side(replayed.iter().map(|(_, r)| *r));
        report.pass_delta = report.replay.pass_rate - report.original.pass_rate;
        report.repeat_delta = i64::from(report.replay.repeats) - i64::from(report.original.repeats);
        Ok(report)
    }

    /// Replays `task` in a sandbox store under the temp dir, removed after.
    async fn replay_task(&self, spec: &EvalSpec, task: &Task, original: &Run) -> Result<Outcome> {
        let dir = std::env::temp_dir().join(format!("sun-eval-{}", TaskId::generate()));
        let result = self.replay_in(&dir, spec, task, original).await;
        let _ = std::fs::remove_dir_all(&dir);
        result
    }

    async fn replay_in(
        &self,
        dir: &Path,
        spec: &EvalSpec,
        task: &Task,
        original: &Run,
    ) -> Result<Outcome> {
        let mut read = self.store.read().await?;
        let workflow_id = task.workflow_id.ok_or_else(|| {
            Error::InvalidInput(format!("task {} has no workflow to replay", task.id))
        })?;
        let workflow = read.get_workflow(workflow_id).await?;
        let yaml = match &spec.candidate.workflow {
            Some(yaml) => yaml.clone(),
            None => {
                read.latest_workflow_version(workflow_id)
                    .await?
                    .ok_or_else(|| {
                        Error::NotFound(format!("workflow {} has no version", workflow.id))
                    })?
                    .source_yaml
            }
        };
        // The sandbox has only workspace Main: the repos go there.
        let mut repos = Vec::new();
        for id in &original.repo_ids {
            repos.push(sun_core::model::Repo {
                workspace_id: sun_core::WorkspaceId::MAIN,
                ..read.get_repo(*id).await?
            });
        }
        let hosts = read.hosts().await?;
        let context = read.task_context(task.id).await?;
        let memory = read.active_memory().await?;
        let recorded = run_events(&mut read, original.id).await?;
        drop(read);
        let transcripts = transcript_events(&recorded);
        let bases = bases(&recorded);
        let decisions = match spec.auto_approve {
            true => Decisions::new(),
            false => decisions(&recorded),
        };
        let parsed = parse_workflow(&yaml)?;

        let sandbox = Store::open(dir).await?;
        let copy = Task {
            id: TaskId::generate(),
            status: TaskStatus::Todo,
            ..task.clone()
        };
        let run = Run {
            id: RunId::generate(),
            task_id: copy.id,
            workflow_id: workflow.id,
            workflow_version: 1,
            status: RunStatus::Queued,
            end_reason: None,
            budget: parsed.budget.clone(),
            repo_ids: original.repo_ids.clone(),
        };
        let mut tx = sandbox.begin().await?;
        // The sandbox has only workspace Main.
        let workflow = sun_core::model::Workflow {
            workspace_id: sun_core::WorkspaceId::MAIN,
            ..workflow
        };
        tx.insert_workflow(&workflow).await?;
        tx.save_workflow_version(workflow.id, &yaml, &parsed.hash)
            .await?;
        for repo in &repos {
            tx.insert_repo(repo).await?;
        }
        for host in &hosts {
            tx.insert_host(host).await?;
        }
        tx.insert_task(&copy).await?;
        tx.put_task_context(copy.id, &context).await?;
        for item in &memory {
            tx.insert_memory(item, &format!("copy:{}", item.id)).await?;
        }
        for (i, line) in spec.candidate.memory.iter().enumerate() {
            tx.insert_memory(&candidate_item(line), &format!("candidate:{i}"))
                .await?;
        }
        tx.insert_run(&run).await?;
        tx.append_event(NewEvent::new(
            run.id,
            None,
            "run_status",
            json!({ "from": null, "to": RunStatus::Queued.as_str(), "reason": null }),
        ))
        .await?;
        tx.commit().await?;

        // Cut the copy's worktree now and move it to the original's base
        // commits; every later prepare finds it in place.
        let worktree = crate::executor::worktree_of(&mut sandbox.read().await?, copy.id).await?;
        let cut = async {
            if bases.is_empty() {
                return Ok(());
            }
            let root = self
                .hosts
                .prepare_worktree(self.host, worktree.clone())
                .await?;
            crate::base::reset(self.hosts.as_ref(), self.host, &worktree, &root, &bases).await
        };
        if let Err(e) = cut.await {
            let _ = self.hosts.remove_worktree(self.host, copy.id).await;
            return Err(e);
        }

        let recorder = Arc::new(Recorder::default());
        let link = Arc::new(Sandbox::new(
            self.hosts.clone(),
            transcripts,
            spec.runtime.is_none(),
        ));
        let config = EngineConfig {
            host: self.host,
            adapter: spec.runtime.clone().unwrap_or_else(|| REPLAY.into()),
            delivery_timeout: Duration::from_secs(5),
            watchdog: WatchdogConfig::default(),
        };
        let (engine, engine_task) =
            Engine::start(sandbox.clone(), link, recorder.clone(), config).await?;
        let ended = engine.drive(run.id, &parsed, TASK_TIMEOUT, decisions).await;
        engine_task.abort();
        // Best effort: a host without worktree support has nothing to remove.
        let _ = self.hosts.remove_worktree(self.host, copy.id).await;
        let ended = ended?;
        let causes = sandbox
            .read()
            .await?
            .corrections_of(copy.id)
            .await?
            .into_iter()
            .map(|c| c.cause_label)
            .collect();
        let actions = recorder.calls.lock().map(|c| c.clone()).unwrap_or_default();
        Ok(Outcome {
            run_id: run.id,
            status: ended.status,
            passed: ended.status == RunStatus::Succeeded,
            end_reason: ended.end_reason,
            causes,
            repeats: 0,
            actions,
        })
    }

    /// Runs a sandbox run to its end: decides each approval as `recorded`
    /// says (in order per step), approves the rest, and cancels the run when
    /// it waits on a human (paused) or takes longer than `timeout`.
    pub(crate) async fn drive(
        &self,
        run_id: RunId,
        workflow: &Workflow,
        timeout: Duration,
        mut recorded: Decisions,
    ) -> Result<Run> {
        let mut events = self.store.subscribe();
        self.start_pinned(run_id).await?;
        let deadline = tokio::time::Instant::now() + timeout;
        let mut approved = HashSet::new();
        loop {
            let mut read = self.store.read().await?;
            let run = read.get_run(run_id).await?;
            if run.status.is_terminal() {
                return Ok(run);
            }
            let steps = read.steps_of_run(run_id).await?;
            drop(read);
            let stuck = match run.status {
                RunStatus::Paused => Some("the replay waited on a human"),
                _ if tokio::time::Instant::now() >= deadline => Some("the replay timed out"),
                _ => None,
            };
            if let Some(why) = stuck {
                let _ = self.cancel(run_id).await;
                let mut run = self.store.read().await?.get_run(run_id).await?;
                run.end_reason = Some(why.into());
                return Ok(run);
            }
            for step in steps
                .iter()
                .filter(|s| s.status == StepStatus::WaitingApproval)
            {
                if !approved.insert(step.id) {
                    continue;
                }
                let by = approver(workflow, &step.step_key);
                let key = step.step_key.clone();
                match recorded.get_mut(&key).and_then(VecDeque::pop_front) {
                    Some(Decision {
                        approved: false,
                        reason: Some(reason),
                    }) => self.reject(run_id, key, by, reason).await?,
                    Some(Decision { approved, .. }) => {
                        self.decide(run_id, key, approved, by).await?
                    }
                    None => self.decide(run_id, key, true, by).await?,
                }
            }
            let _ = tokio::time::timeout(TICK, events.recv()).await;
        }
    }
}

/// The task's last finished run and the causes it was corrected for before
/// that run; `None` when no run of it finished.
async fn original_run(
    read: &mut Tx,
    task_id: TaskId,
) -> Result<Option<(Run, Vec<CorrectionCause>)>> {
    let runs = read.runs_of_task(task_id).await?;
    let Some(index) = runs.iter().rposition(|r| r.status.is_terminal()) else {
        return Ok(None);
    };
    let before: HashSet<RunId> = runs[..index].iter().map(|r| r.id).collect();
    let prior = read
        .corrections_of(task_id)
        .await?
        .into_iter()
        .filter(|c| before.contains(&c.run_id))
        .map(|c| c.cause_label)
        .collect();
    Ok(Some((runs[index].clone(), prior)))
}

/// Corrections among `causes` (one run's, oldest first) whose cause was
/// already corrected: in `prior` runs or earlier in the same run.
fn repeats(causes: &[CorrectionCause], prior: &[CorrectionCause]) -> u32 {
    let mut seen: HashSet<CorrectionCause> = prior.iter().copied().collect();
    let repeated = causes.iter().filter(|c| !seen.insert(**c)).count();
    u32::try_from(repeated).unwrap_or(u32::MAX)
}

fn side<'a>(outcomes: impl Iterator<Item = &'a Outcome>) -> Side {
    let mut side = Side::default();
    let mut total = 0u32;
    for o in outcomes {
        total += 1;
        side.passed += u32::from(o.passed);
        side.mistakes += u32::try_from(o.causes.len()).unwrap_or(u32::MAX);
        side.repeats += o.repeats;
    }
    if total > 0 {
        side.pass_rate = f64::from(side.passed) / f64::from(total);
    }
    side
}

/// Every event of `run_id`, oldest first.
async fn run_events(read: &mut Tx, run_id: RunId) -> Result<Vec<EventRecord>> {
    let mut out = Vec::new();
    loop {
        let after = out.last().map_or(0, |e: &EventRecord| e.seq);
        let page = read.events_after(Some(run_id), after, 500).await?;
        if page.is_empty() {
            return Ok(out);
        }
        out.extend(page);
    }
}

/// Agent events of each step, its first session's only (up to the first
/// `exited`).
fn transcript_events(events: &[EventRecord]) -> HashMap<StepKey, Vec<SunEvent>> {
    let mut out: HashMap<StepKey, Vec<SunEvent>> = HashMap::new();
    let mut ended = HashSet::new();
    for record in events {
        let Some(key) = &record.step_key else {
            continue;
        };
        if ended.contains(key) {
            continue;
        }
        // Engine events have no `kind` field in the payload; agent events do.
        let Ok(event) = serde_json::from_value::<SunEvent>(record.payload.clone()) else {
            continue;
        };
        if matches!(event, SunEvent::Exited { .. }) {
            ended.insert(key.clone());
        }
        out.entry(key.clone()).or_default().push(event);
    }
    out
}

/// The commit each repo was cut from (url → sha): the run's first
/// `run_base`; empty for runs from before it was journaled.
fn bases(events: &[EventRecord]) -> HashMap<String, String> {
    events
        .iter()
        .find(|e| e.kind == crate::base::RUN_BASE)
        .and_then(|e| {
            serde_json::from_value::<Vec<crate::base::RepoBase>>(e.payload["repos"].clone()).ok()
        })
        .unwrap_or_default()
        .into_iter()
        .map(|r| (r.url, r.sha))
        .collect()
}

/// A recorded approval decision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Decision {
    approved: bool,
    reason: Option<String>,
}

/// Recorded decisions per approval step, in the order they were made.
pub(crate) type Decisions = HashMap<StepKey, VecDeque<Decision>>;

/// The `approval` decisions of a run, by humans or by a timeout (replayed
/// as a rejection with the timeout's reason).
fn decisions(events: &[EventRecord]) -> Decisions {
    let mut out = Decisions::new();
    for event in events.iter().filter(|e| e.kind == APPROVAL) {
        let (Some(key), Some(approved)) = (&event.step_key, event.payload["approved"].as_bool())
        else {
            continue;
        };
        let reason = match event.payload["by"].as_str() {
            Some("timeout") => Some("approval timed out".to_owned()),
            _ => event.payload["reason"].as_str().map(str::to_owned),
        };
        out.entry(key.clone())
            .or_default()
            .push_back(Decision { approved, reason });
    }
    out
}

/// Who approves `key` in a replay: its first `who:` user, with its roles.
fn approver(workflow: &Workflow, key: &StepKey) -> Principal {
    let who = workflow
        .steps
        .iter()
        .find(|d| d.id == *key)
        .and_then(|d| match &d.kind {
            StepKind::Approval { who, .. } | StepKind::Decide { who, .. } => Some(who.clone()),
            _ => None,
        })
        .unwrap_or_default();
    Principal {
        user: who
            .iter()
            .find_map(|w| w.strip_prefix("user:"))
            .unwrap_or("eval")
            .to_owned(),
        roles: who
            .iter()
            .filter_map(|w| w.strip_prefix("role:"))
            .map(str::to_owned)
            .collect(),
    }
}

fn candidate_item(line: &str) -> MemoryItem {
    MemoryItem {
        id: MemoryItemId::generate(),
        scope: MemoryScope::Ecosystem,
        scope_id: None,
        body: line.trim().to_owned(),
        target: None,
        reason: "eval candidate".into(),
        source_run: None,
        status: MemoryStatus::Active,
        author: "eval".into(),
        reviewed_by: None,
        created_at: Utc::now(),
    }
}

fn name_of(name: &str) -> Result<String> {
    let name = name.trim();
    if name.is_empty() || name.chars().count() > MAX_NAME || name.contains('\n') {
        return Err(Error::InvalidInput(format!(
            "name: one line of 1–{MAX_NAME} chars"
        )));
    }
    Ok(name.to_owned())
}

fn validate(spec: &EvalSpec) -> Result<()> {
    let lines = &spec.candidate.memory;
    if lines.len() > MAX_LINES
        || lines
            .iter()
            .any(|l| l.trim().is_empty() || l.contains('\n') || l.chars().count() > MAX_LINE)
    {
        return Err(Error::InvalidInput(format!(
            "candidate.memory: up to {MAX_LINES} lines of 1–{MAX_LINE} chars"
        )));
    }
    if let Some(yaml) = &spec.candidate.workflow {
        parse_workflow(yaml)?;
    }
    if let Some(runtime) = &spec.runtime {
        let ok = !runtime.is_empty()
            && runtime.len() <= 64
            && runtime
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
        if !ok || runtime == REPLAY {
            return Err(Error::InvalidInput(format!(
                "runtime `{runtime}`: a runtime name, or none to replay transcripts"
            )));
        }
    }
    if spec.limit.is_some_and(|l| l == 0 || l > MAX_TASKS) {
        return Err(Error::InvalidInput(format!("limit: 1–{MAX_TASKS}")));
    }
    Ok(())
}

pub(crate) fn parse_workflow(yaml: &str) -> Result<Workflow> {
    sun_workflow::parse(yaml).map_err(|errors| {
        let errors: Vec<String> = errors.iter().map(ToString::to_string).collect();
        Error::InvalidInput(format!("candidate workflow: {}", errors.join("; ")))
    })
}

fn protocol(e: serde_json::Error) -> Error {
    Error::Protocol(e.to_string())
}

// ------------------------------------------------------------- the sandbox

/// The sandbox engine's hosts: a **Decorator** over the real `HostLink`.
/// - `replay_all`: every session replays its step's recorded transcript
///   in-kernel instead of starting a CLI.
/// - The push script before an `open_pr` is skipped (succeeds, nothing sent).
/// - Only this sandbox's sessions' events reach its engine.
///
/// Worktrees, files, gates and device checks run on the real host, in the
/// copy's own worktree.
pub(crate) struct Sandbox {
    inner: Arc<dyn HostLink>,
    transcripts: HashMap<StepKey, Vec<SunEvent>>,
    replay_all: bool,
    /// Sessions this sandbox started (forwarded from the inner stream).
    ours: Arc<Mutex<HashSet<SessionId>>>,
    /// Replayed sessions' emitters, stopped on `stop_session`.
    replays: Mutex<HashMap<SessionId, tokio::task::JoinHandle<()>>>,
    sink: Mutex<Option<mpsc::UnboundedSender<HostEvent>>>,
}

impl Sandbox {
    pub(crate) fn new(
        inner: Arc<dyn HostLink>,
        transcripts: HashMap<StepKey, Vec<SunEvent>>,
        replay_all: bool,
    ) -> Self {
        Self {
            inner,
            transcripts,
            replay_all,
            ours: Arc::default(),
            replays: Mutex::default(),
            sink: Mutex::default(),
        }
    }

    fn is_replay(&self, session: SessionId) -> bool {
        self.replays.lock().is_ok_and(|r| r.contains_key(&session))
    }

    fn replay(&self, spec: &SessionSpec) -> SessionHandle {
        let script = self
            .transcripts
            .get(&spec.step_key)
            .cloned()
            .unwrap_or_else(|| {
                vec![
                    SunEvent::Error {
                        message: format!("eval: no recorded transcript for `{}`", spec.step_key),
                    },
                    SunEvent::Exited { code: Some(1) },
                ]
            });
        let sink = self.sink.lock().ok().and_then(|s| s.clone());
        let session_id = spec.session_id;
        let emitter = tokio::spawn(async move {
            let Some(sink) = sink else {
                return;
            };
            for (seq, event) in (1..).zip(script) {
                tokio::time::sleep(EVENT_GAP).await;
                if sink
                    .send(HostEvent {
                        session_id,
                        seq,
                        event,
                    })
                    .is_err()
                {
                    return;
                }
            }
        });
        if let Ok(mut replays) = self.replays.lock() {
            replays.insert(session_id, emitter);
        }
        SessionHandle {
            session_id,
            native_id: None,
            pid: None,
        }
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        if let Ok(replays) = self.replays.lock() {
            replays.values().for_each(tokio::task::JoinHandle::abort);
        }
    }
}

#[async_trait]
impl HostLink for Sandbox {
    async fn start_session(
        &self,
        host: HostId,
        adapter: &str,
        spec: SessionSpec,
    ) -> Result<SessionHandle> {
        if let Ok(mut ours) = self.ours.lock() {
            ours.insert(spec.session_id);
        }
        if self.replay_all || adapter == REPLAY {
            return Ok(self.replay(&spec));
        }
        self.inner.start_session(host, adapter, spec).await
    }

    async fn steer(&self, session: &SessionHandle, msg: Steer) -> Result<()> {
        match self.is_replay(session.session_id) {
            true => Ok(()),
            false => self.inner.steer(session, msg).await,
        }
    }

    async fn stop_session(&self, session: &SessionHandle, how: StopMode) -> Result<()> {
        let replayed = self
            .replays
            .lock()
            .ok()
            .and_then(|r| {
                r.get(&session.session_id)
                    .map(tokio::task::JoinHandle::abort)
            })
            .is_some();
        match replayed {
            true => Ok(()),
            false => self.inner.stop_session(session, how).await,
        }
    }

    async fn is_alive(&self, session: &SessionHandle) -> Result<bool> {
        match self.is_replay(session.session_id) {
            true => Ok(false),
            false => self.inner.is_alive(session).await,
        }
    }

    fn events(&self) -> BoxStream<'static, HostEvent> {
        let (sink, source) = mpsc::unbounded_channel();
        let mut inner = self.inner.events();
        let (forward, ours) = (sink.clone(), self.ours.clone());
        tokio::spawn(async move {
            while let Some(event) = crate::executor::next(&mut inner).await {
                let mine = ours.lock().is_ok_and(|o| o.contains(&event.session_id));
                if mine && forward.send(event).is_err() {
                    return;
                }
                if forward.is_closed() {
                    return;
                }
            }
        });
        if let Ok(mut slot) = self.sink.lock() {
            *slot = Some(sink);
        }
        Box::pin(Events(source))
    }

    async fn ack(&self, session: SessionId, seq: u64) -> Result<()> {
        match self.is_replay(session) {
            true => Ok(()),
            false => self.inner.ack(session, seq).await,
        }
    }

    fn is_online(&self, host: HostId) -> bool {
        self.inner.is_online(host)
    }

    async fn prepare_worktree(&self, host: HostId, worktree: Worktree) -> Result<PathBuf> {
        self.inner.prepare_worktree(host, worktree).await
    }

    async fn remove_worktree(&self, host: HostId, task_id: TaskId) -> Result<()> {
        self.inner.remove_worktree(host, task_id).await
    }

    async fn write_files(&self, host: HostId, cwd: PathBuf, files: Vec<SessionFile>) -> Result<()> {
        self.inner.write_files(host, cwd, files).await
    }

    async fn exec(&self, host: HostId, request: ExecRequest) -> Result<ExecOutput> {
        if request.command.contains(crate::push::PUSH) {
            return Ok(ExecOutput {
                code: Some(0),
                output: "eval replay: push skipped".into(),
                timed_out: false,
            });
        }
        self.inner.exec(host, request).await
    }

    async fn verify_on_device(&self, host: HostId, request: DeviceRequest) -> Result<DeviceResult> {
        self.inner.verify_on_device(host, request).await
    }

    async fn stats(&self, session: SessionId) -> Result<SessionStats> {
        self.inner.stats(session).await
    }
}

struct Events(mpsc::UnboundedReceiver<HostEvent>);

impl futures_core::Stream for Events {
    type Item = HostEvent;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<HostEvent>> {
        self.0.poll_recv(cx)
    }
}

/// The sandbox engine's mods: records each outbox action, delivers nothing.
#[derive(Default)]
pub(crate) struct Recorder {
    pub(crate) calls: Mutex<Vec<String>>,
}

#[async_trait]
impl ModLink for Recorder {
    async fn call(&self, call: ModCall) -> Result<Value> {
        if let Ok(mut calls) = self.calls.lock() {
            calls.push(format!("{}.{}", call.mod_name, call.method));
        }
        Ok(json!({ "recorded": true }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn eval_repeats_count_known_and_same_run_causes() {
        use CorrectionCause::*;
        assert_eq!(repeats(&[TestFailure, TestFailure, Lint], &[]), 1);
        assert_eq!(repeats(&[Lint], &[Lint]), 1);
        assert_eq!(repeats(&[], &[Lint]), 0);
    }

    #[test]
    fn eval_spec_parses_and_validates() {
        let spec: EvalSpec = serde_json::from_value(json!({
            "name": "tests first",
            "pick": {"by": "cause", "cause": "test_failure"},
            "candidate": {"memory": ["Run the tests before claiming done."]}
        }))
        .unwrap_or_else(|e| panic!("{e}"));
        assert!(validate(&spec).is_ok());
        let bad = EvalSpec {
            runtime: Some(REPLAY.into()),
            ..spec.clone()
        };
        assert!(validate(&bad).is_err());
        let bad = EvalSpec {
            candidate: Candidate {
                workflow: Some("steps: [".into()),
                memory: vec![],
            },
            ..spec
        };
        assert!(validate(&bad).is_err());
    }
}
