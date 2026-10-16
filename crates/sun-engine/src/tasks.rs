//! The task lifecycle: the one owner of task status moves (the
//! `sun_core::TaskStatus` **State machine**). Child module of `executor`.
//!
//! - **The engine** moves a task that runs something after its latest run
//!   ([`follow_run`], from every run and step move): run queued → Todo, an
//!   agent step working → In progress, verifying or a gate running →
//!   Verifying, waiting approval or a decision → Needs you, succeeded → Done
//!   when the workflow has an approval step (a person reviewed), else Needs
//!   you (a person marks it Done), failed/stalled/looping/over budget →
//!   Failed, cancelled → Cancelled; paused by a person (Stop, Pause, a move,
//!   a killed session) → Blocked, paused for a person to decide (a limit's
//!   `pause_and_notify`, a raised flag) → Needs you.
//!   A move the machine has no edge for goes through the status it needs
//!   (Needs you before Done, Todo out of Blocked or Failed); a task a person
//!   parked in Backlog or Blocked stays there while its run pauses or is
//!   cancelled.
//! - **A person** moves a task with [`Engine::move_task_manually`]: → Backlog
//!   or Blocked pauses a running run (a queued one is cancelled); → Todo or
//!   In progress resumes a paused run or queues a new one; → Cancelled or
//!   Failed cancels the run. Verifying is the engine's for a task that runs
//!   something; a plain task only changes status. Done waits until the run
//!   has ended (approve or cancel it first).
//! - Every move, change and comment appends a `task_activity` row in the
//!   same transaction (the store broadcasts it on commit).
//! - **Agent triggers.** A task assigned to an agent runs the agent's system
//!   workflow (`agents`), so the rules above start it: created in Todo or
//!   moved out of Backlog queues a run. A comment ([`Engine::comment`])
//!   steers its running agent (with no live session, the next one gets it);
//!   on a queued run it waits for the run's first session; on a Blocked,
//!   Needs you or Failed task it moves the task to Todo, which resumes the
//!   paused run or queues a new one, the comment going to its next session.
//!   The engine handles comments one at a time, so a storm of them finds
//!   the run the first one queued and queues no other. A comment that
//!   @mentions agents wakes only them (`conversation`).
//! - **Chats** (`TaskKind::Chat`) follow the same rules; they start in
//!   Needs you, so their first message queues their first run.
//! - **Assigning** ([`Engine::assign_task`]) cancels the task's run going
//!   (it no longer moves the task) and, on an open task out of Backlog,
//!   queues one for the new assignee.

use std::collections::HashMap;
use std::sync::{Arc, LazyLock, Mutex, PoisonError};

use serde_json::{Value, json};
use sun_core::model::{
    ActivityKind, Assignee, Priority, Run, Task, TaskActivity, TaskKey, TaskKind,
};
use sun_core::{
    AgentId, Error, ExecEnvSpec, MemberId, ProjectId, Result, RunId, RunStatus, StateMachine,
    StepStatus, TaskId, TaskStatus, WorkflowId, WorkspaceId,
};
use sun_store::{
    AgentRepo, EventLog, NewEvent, RunRepo, SettingRepo, TaskRepo, Tx, WorkflowRepo, WorkspaceRepo,
};
use sun_workflow::StepKind;
use tokio::sync::broadcast;

use super::{Command, Engine, Executor, pause, state};
use crate::{agents, conversation, queue, queued};

/// Who did something to a task: a person (signed in, or the token holder,
/// who has no member id) as the audit names them, e.g. `user:ana`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Actor {
    /// Their member row; `None` for the token holder.
    pub member_id: Option<MemberId>,
    /// The audit actor (`Caller::actor` in the API).
    pub name: String,
}

/// Appends an entry to the task's activity by `actor` (`None`: the
/// engine). A person's entry names them in its body (`by`), so one without
/// a member id is never read as the engine's.
pub(crate) async fn log(
    tx: &mut Tx,
    task_id: TaskId,
    kind: ActivityKind,
    actor: Option<&Actor>,
    mut body: Value,
) -> Result<TaskActivity> {
    if let (Some(actor), Value::Object(map)) = (actor, &mut body) {
        map.insert("by".into(), Value::String(actor.name.clone()));
    }
    let member = actor.and_then(|a| a.member_id);
    Ok(tx.append_activity(task_id, kind, member, body).await?)
}

/// Moves the task to `to` (nothing when it is there already) with its
/// activity row, by `actor` (`None`: the engine), journaled in `run`'s log
/// too when the move is about a run (the `task_created` hook reads it).
pub(crate) async fn set_status(
    tx: &mut Tx,
    task_id: TaskId,
    to: TaskStatus,
    actor: Option<&Actor>,
    run: Option<RunId>,
) -> Result<()> {
    let from = tx.get_task(task_id).await?.status;
    if from == to {
        return Ok(());
    }
    tx.set_task_status(task_id, to).await?;
    let moved = json!({ "from": from, "to": to });
    log(tx, task_id, ActivityKind::Status, actor, moved).await?;
    if let Some(run_id) = run {
        let payload = json!({ "task_id": task_id, "from": from, "to": to });
        tx.append_event(NewEvent::new(run_id, None, "task_status", payload))
            .await?;
    }
    Ok(())
}

/// Moves the run's task to where its run is now, in the caller's
/// transaction (see the module doc).
pub(crate) async fn follow_run(tx: &mut Tx, run_id: RunId) -> Result<()> {
    let run = tx.get_run(run_id).await?;
    let task = tx.get_task(run.task_id).await?;
    // A run of the task's old assignee (cancelled on reassignment).
    if task.workflow_id != Some(run.workflow_id) {
        return Ok(());
    }
    let parked = matches!(task.status, TaskStatus::Backlog | TaskStatus::Blocked)
        && matches!(run.status, RunStatus::Paused | RunStatus::Cancelled);
    if parked {
        return Ok(());
    }
    let Some(next) = derived(tx, &run).await? else {
        return Ok(());
    };
    for step in path(task.status, next) {
        set_status(tx, task.id, step, None, Some(run_id)).await?;
    }
    Ok(())
}

/// The status the engine → status mapping gives a task whose run is `run`;
/// `None` while a running run is between steps (it keeps its status, so a
/// step finishing doesn't flicker it).
async fn derived(tx: &mut Tx, run: &Run) -> Result<Option<TaskStatus>> {
    use StepStatus::*;
    Ok(Some(match run.status {
        RunStatus::Queued => TaskStatus::Todo,
        RunStatus::Running => {
            let steps = state::latest(tx.steps_of_run(run.id).await?);
            let doing = |statuses: &[StepStatus]| {
                steps
                    .values()
                    .filter(|s| statuses.contains(&s.status))
                    .collect::<Vec<_>>()
            };
            if !doing(&[WaitingApproval]).is_empty() {
                TaskStatus::NeedsYou
            } else if !doing(&[Verifying]).is_empty() {
                TaskStatus::Verifying
            } else {
                let working = doing(&[
                    Dispatched,
                    Running,
                    LimitHit,
                    HandingOff,
                    Stalled,
                    Interrupted,
                ]);
                if working.is_empty() {
                    return Ok(None);
                }
                // A gate step runs as `running`: its kind says it verifies.
                let kinds = step_kinds(tx, run).await?;
                let gate = |key: &sun_core::StepKey| {
                    kinds
                        .iter()
                        .any(|(k, kind)| k == key && *kind == Kind::Gate)
                };
                match working
                    .iter()
                    .any(|s| s.status == Running && gate(&s.step_key))
                {
                    true => TaskStatus::Verifying,
                    false => TaskStatus::InProgress,
                }
            }
        }
        RunStatus::Paused if pause::by_a_person(run.end_reason.as_deref()) => TaskStatus::Blocked,
        RunStatus::Paused => TaskStatus::NeedsYou,
        RunStatus::Succeeded => {
            let kinds = step_kinds(tx, run).await?;
            match kinds.iter().any(|(_, k)| *k == Kind::Approval) {
                true => TaskStatus::Done,
                false => TaskStatus::NeedsYou,
            }
        }
        RunStatus::Cancelled => TaskStatus::Cancelled,
        RunStatus::Failed | RunStatus::Stalled | RunStatus::Looping | RunStatus::BudgetExceeded => {
            TaskStatus::Failed
        }
    }))
}

/// The run's pinned workflow steps (key, kind); none when it doesn't parse.
/// What [`derived`] needs to know of a step's kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Gate,
    Approval,
    Other,
}

type Kinds = Arc<Vec<(sun_core::StepKey, Kind)>>;

/// Parsed step kinds by workflow source. Keyed by the YAML itself, not the
/// version: an eval sandbox saves its candidate as another store's version
/// 1. One entry per distinct source.
static KINDS: LazyLock<Mutex<HashMap<String, Kinds>>> = LazyLock::new(Mutex::default);

/// The step kinds of the run's pinned workflow version (none when it
/// doesn't parse), parsed once per source.
async fn step_kinds(tx: &mut Tx, run: &Run) -> Result<Kinds> {
    let source = tx
        .get_workflow_version(run.workflow_id, run.workflow_version)
        .await?
        .source_yaml;
    let mut cache = KINDS.lock().unwrap_or_else(PoisonError::into_inner);
    if let Some(kinds) = cache.get(&source) {
        return Ok(kinds.clone());
    }
    let kind = |kind: &StepKind| match kind {
        StepKind::Gate { .. } => Kind::Gate,
        StepKind::Approval { .. } => Kind::Approval,
        _ => Kind::Other,
    };
    let kinds: Kinds = Arc::new(
        sun_workflow::parse(&source)
            .map(|w| {
                w.steps
                    .iter()
                    .map(|s| (s.id.clone(), kind(&s.kind)))
                    .collect()
            })
            .unwrap_or_default(),
    );
    cache.insert(source, kinds.clone());
    Ok(kinds)
}

/// The moves that take a task from `from` to `to` for the engine: the edge
/// itself, else through the status the machine needs; none when there is
/// no way (Done, Cancelled).
fn path(from: TaskStatus, to: TaskStatus) -> Vec<TaskStatus> {
    use TaskStatus::*;
    if from == to {
        Vec::new()
    } else if from.can_transition_to(to) {
        vec![to]
    } else if to == Done && from.is_open() {
        vec![NeedsYou, Done]
    } else if matches!(from, Blocked | Failed) && to.is_open() {
        vec![Todo, to]
    } else {
        Vec::new()
    }
}

/// What a person's move does to the task's run.
enum RunAction {
    None,
    Pause(RunId),
    Resume(RunId),
    Queue,
    Cancel(RunId),
}

impl Executor {
    /// See [`Engine::move_task_manually`]. `message` (a comment that woke
    /// an agent) goes to the next session of the run it resumes or queues.
    pub(super) async fn move_task(
        &mut self,
        task_id: TaskId,
        to: TaskStatus,
        actor: Option<Actor>,
        message: Option<String>,
    ) -> Result<()> {
        let mut tx = self.store.begin().await?;
        let task = tx.get_task(task_id).await?;
        if task.status == to {
            return Ok(());
        }
        if to == TaskStatus::Verifying && task.runs_something() {
            return Err(Error::InvalidTransition {
                machine: TaskStatus::NAME,
                from: task.status.as_str(),
                to: to.as_str(),
            });
        }
        task.status.transition(to)?;
        let active = tx
            .runs_of_task(task_id)
            .await?
            .pop()
            .filter(|r| !r.status.is_terminal());
        // Done never leaves a run going behind it.
        if to == TaskStatus::Done && active.is_some() {
            return Err(Error::InvalidTransition {
                machine: TaskStatus::NAME,
                from: task.status.as_str(),
                to: to.as_str(),
            });
        }
        let action = match (to, active.as_ref().map(|r| (r.id, r.status))) {
            (TaskStatus::Backlog | TaskStatus::Blocked, Some((run, RunStatus::Running))) => {
                RunAction::Pause(run)
            }
            // A queued run can't pause; parked, it waits for a new run.
            (TaskStatus::Backlog | TaskStatus::Blocked, Some((run, RunStatus::Queued))) => {
                RunAction::Cancel(run)
            }
            (TaskStatus::Todo | TaskStatus::InProgress, Some((run, RunStatus::Paused))) => {
                RunAction::Resume(run)
            }
            (TaskStatus::Todo | TaskStatus::InProgress, None) if task.workflow_id.is_some() => {
                RunAction::Queue
            }
            (TaskStatus::Cancelled | TaskStatus::Failed, Some((run, _))) => RunAction::Cancel(run),
            _ => RunAction::None,
        };
        // A queued run means Todo (the mapping), whatever column it was dropped on.
        let queued = matches!(action, RunAction::Queue)
            || active
                .as_ref()
                .is_some_and(|r| r.status == RunStatus::Queued);
        let to = match (to, queued) {
            (TaskStatus::InProgress, true) => TaskStatus::Todo,
            _ => to,
        };
        // The person's move first: the run's moves then find the task where
        // they put it (see `follow_run`).
        let run = active.as_ref().map(|r| r.id);
        set_status(&mut tx, task_id, to, actor.as_ref(), run).await?;
        let mut effects = Vec::new();
        match action {
            RunAction::None => {}
            RunAction::Pause(run) => {
                self.pause_in(&mut tx, run, pause::PAUSED, &mut effects)
                    .await?;
            }
            RunAction::Resume(run) => {
                if let Some(text) = &message {
                    queued::leave(&mut tx, run, text).await?;
                }
                self.resume_in(&mut tx, run, &mut effects).await?;
            }
            RunAction::Queue => {
                let trigger = if message.is_some() {
                    conversation::COMMENT
                } else {
                    "moved"
                };
                let run = queue::queue_run(&mut tx, task_id, trigger).await?;
                if let Some(text) = &message {
                    queued::leave(&mut tx, run, text).await?;
                }
            }
            RunAction::Cancel(run) => {
                let mut latest = state::latest(tx.steps_of_run(run).await?);
                let why = "cancelled by a human";
                self.end_run(
                    &mut tx,
                    run,
                    &mut latest,
                    RunStatus::Cancelled,
                    why,
                    false,
                    &mut effects,
                )
                .await?;
            }
        }
        tx.commit().await?;
        self.apply(effects).await
    }

    /// See [`Engine::assign_task`].
    pub(super) async fn assign_task(
        &mut self,
        task_id: TaskId,
        to: Option<Assignee>,
        actor: Option<Actor>,
    ) -> Result<()> {
        let mut tx = self.store.begin().await?;
        let mut task = tx.get_task(task_id).await?;
        let before = task.assignee();
        if before == to {
            return Ok(());
        }
        if task.status == TaskStatus::Done {
            return Err(Error::InvalidInput(
                "This task is done; its assignee stays.".into(),
            ));
        }
        let runs = tx.runs_of_task(task_id).await?;
        (task.assignee_agent_id, task.workflow_id) = match to {
            None => (None, None),
            Some(Assignee::Agent { id }) => {
                let workflow = agents::system_workflow(&mut tx, id, task.workspace_id).await?;
                (Some(id), Some(workflow))
            }
            Some(Assignee::Workflow { id }) => {
                if !runs.is_empty() {
                    return Err(Error::InvalidInput(
                        "A task that has run keeps its workflow; assign an agent or create a new task."
                            .into(),
                    ));
                }
                (None, Some(person_workflow(&mut tx, id).await?))
            }
        };
        tx.update_task(&task).await?;
        let moved = json!({ "from": before, "to": to });
        log(
            &mut tx,
            task_id,
            ActivityKind::Assignee,
            actor.as_ref(),
            moved,
        )
        .await?;
        let mut effects = Vec::new();
        if let Some(run) = runs.last().filter(|r| !r.status.is_terminal()) {
            let mut latest = state::latest(tx.steps_of_run(run.id).await?);
            self.end_run(
                &mut tx,
                run.id,
                &mut latest,
                RunStatus::Cancelled,
                "the task was reassigned",
                false,
                &mut effects,
            )
            .await?;
        }
        let working = matches!(
            task.status,
            TaskStatus::Todo
                | TaskStatus::InProgress
                | TaskStatus::Verifying
                | TaskStatus::NeedsYou
        );
        if task.workflow_id.is_some() && working {
            queue::queue_run(&mut tx, task_id, "assigned").await?;
        }
        tx.commit().await?;
        self.apply(effects).await
    }

    /// A comment on a task assigned to an active agent (see the module doc).
    pub(super) async fn on_comment(
        &mut self,
        task_id: TaskId,
        text: String,
        actor: Option<Actor>,
    ) -> Result<()> {
        let mut tx = self.store.read().await?;
        let task = tx.get_task(task_id).await?;
        let Some(agent) = task.assignee_agent_id else {
            return Ok(());
        };
        if tx.get_agent(agent).await?.archived_at.is_some() {
            return Ok(());
        }
        let active = tx
            .runs_of_task(task_id)
            .await?
            .pop()
            .filter(|r| !r.status.is_terminal());
        drop(tx);
        // A message left for a later session says who wrote it.
        let left = match &actor {
            Some(actor) => format!("{}: {text}", conversation::person(&actor.name)),
            None => text.clone(),
        };
        match active.map(|r| (r.id, r.status)) {
            Some((run, RunStatus::Running)) => match self.steer(run, text).await {
                // No live agent session: the next one gets it.
                Err(Error::InvalidInput(_)) => self.leave(run, &left).await,
                steered => steered.map(drop),
            },
            Some((run, RunStatus::Queued)) => self.leave(run, &left).await,
            _ if matches!(
                task.status,
                TaskStatus::Blocked | TaskStatus::NeedsYou | TaskStatus::Failed
            ) =>
            {
                self.move_task(task_id, TaskStatus::Todo, actor, Some(left))
                    .await
            }
            _ => Ok(()),
        }
    }

    /// Leaves `text` for the run's next agent session.
    async fn leave(&mut self, run: RunId, text: &str) -> Result<()> {
        let mut tx = self.store.begin().await?;
        queued::leave(&mut tx, run, text).await?;
        Ok(tx.commit().await?)
    }
}

/// `id` when it is a person's workflow with a saved version; an agent's
/// system workflow answers like a missing one.
async fn person_workflow(tx: &mut Tx, id: WorkflowId) -> Result<WorkflowId> {
    let system = tx.system_agent_of(id).await?.is_some();
    if system || tx.latest_workflow_version(id).await?.is_none() {
        return Err(Error::NotFound(format!("workflow {id}")));
    }
    Ok(id)
}

/// `POST /tasks`: a new task.
#[derive(Debug, Clone)]
pub struct NewTask {
    /// The workspace; required without a workflow (or for its default one),
    /// else it must be the workflow's.
    pub workspace_id: Option<WorkspaceId>,
    /// Title (1–200 characters).
    pub title: String,
    /// Description.
    pub body: String,
    /// What it runs.
    pub workflow: WorkflowPick,
    /// Overrides the workflow's `exec_env:`.
    pub exec_env: Option<ExecEnvSpec>,
    /// Priority.
    pub priority: Priority,
    /// `backlog` or `todo`: a task with a workflow queues a run only in `todo`.
    pub status: TaskStatus,
    /// A project on the task's workspace.
    pub project_id: Option<ProjectId>,
    /// Who creates it; `None`: the engine.
    pub actor: Option<Actor>,
}

/// The workflow a new task runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkflowPick {
    /// None: a plain task that never runs.
    None,
    /// The workspace's default workflow.
    Default,
    /// This one.
    Id(WorkflowId),
    /// This agent: its system workflow on the task's workspace.
    Agent(AgentId),
}

/// `PATCH /tasks/{id}`: the fields to change; absent ones stay.
#[derive(Debug, Clone, Default, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskChange {
    /// New title (1–200 characters).
    pub title: Option<String>,
    /// New description.
    pub body: Option<String>,
    /// A move ([`Engine::move_task_manually`]).
    pub status: Option<TaskStatus>,
    /// New priority.
    pub priority: Option<Priority>,
    /// `Some(None)` (JSON `null`) takes it out of its project.
    #[serde(default, deserialize_with = "crate::workspaces::present")]
    pub project_id: Option<Option<ProjectId>>,
    /// Who works on it ([`Engine::assign_task`]); `Some(None)` (JSON
    /// `null`): nobody, a plain task.
    #[serde(default, deserialize_with = "crate::workspaces::present")]
    pub assignee: Option<Option<Assignee>>,
    /// New place in its column.
    pub position: Option<f64>,
}

impl Engine {
    /// Moves a task as a person (`actor`) dragged it (see the module doc).
    /// An illegal move is `InvalidTransition` and changes nothing.
    pub async fn move_task_manually(
        &self,
        task_id: TaskId,
        to: TaskStatus,
        actor: Option<Actor>,
    ) -> Result<()> {
        self.request(|reply| Command::MoveTask {
            task_id,
            to,
            actor,
            reply,
        })
        .await
    }

    /// Assigns the task to an agent, to a workflow of its workspace (only
    /// while it hasn't run), or to nobody (`None`), as `actor`. A run going
    /// is cancelled; an open task out of Backlog queues a run for the new
    /// assignee. A Done task keeps its assignee.
    pub async fn assign_task(
        &self,
        task_id: TaskId,
        to: Option<Assignee>,
        actor: Option<Actor>,
    ) -> Result<()> {
        self.request(|reply| Command::AssignTask {
            task_id,
            to,
            actor,
            reply,
        })
        .await
    }

    /// Stop: pauses the task's running run (its agent sessions stop) and the
    /// task goes Blocked. The caller checks that something runs.
    pub async fn interrupt(&self, task_id: TaskId, actor: Option<Actor>) -> Result<()> {
        self.move_task_manually(task_id, TaskStatus::Blocked, actor)
            .await
    }

    /// Creates a task (see [`NewTask`]) numbered on its workspace, with its
    /// `created` activity, and queues its run when it has a workflow and is `todo`.
    pub async fn create_task(&self, new: NewTask) -> Result<Task> {
        let title = Task::clean_title(&new.title)?;
        if let Some(env) = &new.exec_env {
            env.validate()?;
        }
        if !matches!(new.status, TaskStatus::Backlog | TaskStatus::Todo) {
            return Err(Error::InvalidInput(
                "A new task starts in Backlog or Todo.".into(),
            ));
        }
        let mut tx = self.store.begin().await?;
        let pick_workspace = || {
            new.workspace_id
                .ok_or_else(|| Error::InvalidInput("Pick a workspace for the task.".into()))
        };
        let mut agent = None;
        let (workspace_id, workflow_id) = match new.workflow {
            WorkflowPick::None => (pick_workspace()?, None),
            WorkflowPick::Agent(id) => {
                let workspace = pick_workspace()?;
                agent = Some(id);
                let workflow = agents::system_workflow(&mut tx, id, workspace).await?;
                (workspace, Some(workflow))
            }
            WorkflowPick::Default => {
                let workspace = tx.get_workspace(pick_workspace()?).await?;
                let workflow = workspace.default_workflow_id.ok_or_else(|| {
                    Error::InvalidInput("This workspace has no default workflow.".into())
                })?;
                (workspace.id, Some(workflow))
            }
            WorkflowPick::Id(id) => {
                let id = person_workflow(&mut tx, id).await?;
                let on = tx.get_workflow(id).await?.workspace_id;
                if new.workspace_id.is_some_and(|w| w != on) {
                    return Err(Error::InvalidInput(
                        "The workflow is on another workspace than the task.".into(),
                    ));
                }
                (on, Some(id))
            }
        };
        if let Some(workflow) = workflow_id
            && tx.latest_workflow_version(workflow).await?.is_none()
        {
            return Err(Error::NotFound(format!("workflow {workflow}")));
        }
        let queue = workflow_id.is_some() && new.status == TaskStatus::Todo;
        // Queued from Backlog, so the run journals `backlog → todo` (the
        // `task_created` hook).
        let status = if queue {
            TaskStatus::Backlog
        } else {
            new.status
        };
        let task = Task {
            body: new.body,
            workflow_id,
            priority: new.priority,
            status,
            assignee_agent_id: agent,
            project_id: new.project_id,
            created_by: new.actor.as_ref().and_then(|a| a.member_id),
            ..Task::new(workspace_id, title)
        };
        tx.insert_task(&task).await?;
        let created = json!({ "status": status });
        log(
            &mut tx,
            task.id,
            ActivityKind::Created,
            new.actor.as_ref(),
            created,
        )
        .await?;
        if let Some(env) = new.exec_env {
            let key = super::task_exec_env_key(task.id);
            tx.put_setting(&key, &json!(env)).await?;
        }
        if queue {
            queue::queue_run(&mut tx, task.id, "created").await?;
        }
        let task = tx.get_task(task.id).await?;
        tx.commit().await?;
        Ok(task)
    }

    /// Changes the fields `change` names, each with its activity row; a
    /// status goes first, through [`Engine::move_task_manually`].
    pub async fn update_task(
        &self,
        task_id: TaskId,
        change: TaskChange,
        actor: Option<Actor>,
    ) -> Result<Task> {
        let title = change.title.as_deref().map(Task::clean_title).transpose()?;
        if change.position.is_some_and(|p| !p.is_finite()) {
            return Err(Error::InvalidInput("The position must be a number.".into()));
        }
        if let Some(assignee) = change.assignee {
            self.assign_task(task_id, assignee, actor.clone()).await?;
        }
        if let Some(status) = change.status {
            self.move_task_manually(task_id, status, actor.clone())
                .await?;
        }
        let mut tx = self.store.begin().await?;
        let before = tx.get_task(task_id).await?;
        let mut task = before.clone();
        let mut changes: Vec<(ActivityKind, Value)> = Vec::new();
        if let Some(title) = title.filter(|t| *t != before.title) {
            changes.push((
                ActivityKind::Title,
                json!({ "from": before.title, "to": title }),
            ));
            task.title = title;
        }
        if let Some(body) = change.body.filter(|b| *b != before.body) {
            changes.push((ActivityKind::Description, json!({})));
            task.body = body;
        }
        if let Some(priority) = change.priority.filter(|p| *p != before.priority) {
            let moved = json!({ "from": before.priority, "to": priority });
            changes.push((ActivityKind::Priority, moved));
            task.priority = priority;
        }
        if let Some(project) = change.project_id.filter(|p| *p != before.project_id) {
            let moved = json!({ "from": before.project_id, "to": project });
            changes.push((ActivityKind::Project, moved));
            task.project_id = project;
        }
        if let Some(position) = change.position {
            task.position = position;
        }
        if task == before {
            return Ok(before);
        }
        tx.update_task(&task).await?;
        for (kind, body) in changes {
            log(&mut tx, task_id, kind, actor.as_ref(), body).await?;
        }
        let task = tx.get_task(task_id).await?;
        tx.commit().await?;
        Ok(task)
    }

    /// Sets a task's priority as the engine (triage, the old `bump`), with its activity.
    pub async fn set_task_priority(&self, id: TaskId, priority: Priority) -> Result<()> {
        let mut tx = self.store.begin().await?;
        set_priority(&mut tx, id, priority, None).await?;
        Ok(tx.commit().await?)
    }

    /// A person's comment on the task (1–10 000 characters), mentioning
    /// `mentions` (`conversation::mentioned`). It steers or wakes the
    /// mentioned agents (each on the task if it is the assignee, else in its
    /// mention chat), or with no mentions the task's agent (see the module
    /// doc); a wake that fails is logged, and the comment stays.
    pub async fn comment(
        &self,
        task_id: TaskId,
        text: &str,
        actor: Option<Actor>,
        mentions: &[AgentId],
    ) -> Result<TaskActivity> {
        let text = TaskActivity::clean_comment(text)?;
        let mut tx = self.store.begin().await?;
        let task = tx.get_task(task_id).await?;
        let mentions = conversation::mentioned(&mut tx, &task, mentions).await?;
        let mut comment = json!({ "text": text });
        if !mentions.is_empty() {
            comment["mentions"] = json!(mentions);
        }
        let activity = log(
            &mut tx,
            task_id,
            ActivityKind::Comment,
            actor.as_ref(),
            comment,
        )
        .await?;
        let mut wake = Vec::new();
        if mentions.is_empty() && task.assignee_agent_id.is_some() {
            wake.push(task_id);
        }
        for agent in mentions {
            wake.push(match task.assignee_agent_id == Some(agent) {
                true => task_id,
                false => conversation::mention_chat(&mut tx, &task, agent).await?,
            });
        }
        tx.commit().await?;
        for task_id in wake {
            let (text, actor) = (text.clone(), actor.clone());
            let woken = self
                .request(|reply| Command::Comment {
                    task_id,
                    text,
                    actor,
                    reply,
                })
                .await;
            if let Err(e) = woken {
                eprintln!("sun: task {task_id}: the comment didn't reach its agent: {e}");
            }
        }
        Ok(activity)
    }

    /// The task's activity with `seq > after`, oldest first.
    pub async fn activity(&self, task_id: TaskId, after: i64) -> Result<Vec<TaskActivity>> {
        let mut tx = self.store.read().await?;
        tx.get_task(task_id).await?;
        Ok(tx.activity_of(task_id, after).await?)
    }

    /// Every task activity entry committed from now on (**Observer**).
    /// Subscribe before paging [`Engine::activity`]; on `Lagged`, page again.
    pub fn subscribe_activity(&self) -> broadcast::Receiver<TaskActivity> {
        self.store.subscribe_activity()
    }

    /// The id of the task `key` names (its prefix in any case), or `NotFound`.
    pub async fn task_id_by_key(&self, key: &TaskKey) -> Result<TaskId> {
        Ok(self.store.read().await?.task_by_key(key).await?.id)
    }
}

/// Sets the task's priority with its activity row, in the caller's transaction.
pub(crate) async fn set_priority(
    tx: &mut Tx,
    task_id: TaskId,
    priority: Priority,
    actor: Option<&Actor>,
) -> Result<()> {
    let mut task = tx.get_task(task_id).await?;
    if task.priority == priority {
        return Ok(());
    }
    let moved = json!({ "from": task.priority, "to": priority });
    task.priority = priority;
    tx.update_task(&task).await?;
    log(tx, task_id, ActivityKind::Priority, actor, moved).await?;
    Ok(())
}

/// The task's key, `<PREFIX>-<number>` on its workspace; `InvalidInput` for a chat.
pub(crate) async fn key_of(tx: &mut Tx, task: &Task) -> Result<TaskKey> {
    let prefix = tx.get_workspace(task.workspace_id).await?.issue_prefix;
    task.key(prefix)
}

/// The task's git branch, `sun/<KEY>-<slug>` ([`TaskKey::branch`]): what
/// the host checks out and every PR, review and merge is matched on. A
/// chat has no key, and no repos, but its runs still read one
/// (`outbox::RunFacts`): `sun/chat-<id>`, which no key parses from.
pub(crate) async fn branch_of(tx: &mut Tx, task: &Task) -> Result<String> {
    if task.kind == TaskKind::Chat {
        return Ok(format!("sun/chat-{}", task.id));
    }
    Ok(key_of(tx, task).await?.branch(&task.title))
}

/// The task whose branch `branch` is ([`TaskKey::from_branch`]); `None`
/// for a branch Sun didn't cut for a task, or a key no task has.
pub(crate) async fn task_of_branch(tx: &mut Tx, branch: &str) -> Result<Option<TaskId>> {
    let Some(key) = TaskKey::from_branch(branch) else {
        return Ok(None);
    };
    match tx.task_by_key(&key).await {
        Ok(task) => Ok(Some(task.id)),
        Err(e) => match Error::from(e) {
            Error::NotFound(_) => Ok(None),
            e => Err(e),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn engine_paths_go_through_the_status_the_machine_needs() {
        use TaskStatus::*;
        assert_eq!(path(Todo, InProgress), [InProgress]);
        assert_eq!(path(InProgress, Done), [NeedsYou, Done]);
        assert_eq!(path(NeedsYou, Done), [Done]);
        assert_eq!(path(Blocked, InProgress), [Todo, InProgress]);
        assert_eq!(path(Failed, Verifying), [Todo, Verifying]);
        assert!(path(Done, InProgress).is_empty(), "Done is terminal");
        assert!(path(Cancelled, InProgress).is_empty());
        assert!(path(Failed, Cancelled).is_empty());
        assert!(path(InProgress, InProgress).is_empty());
        for &from in TaskStatus::ALL {
            for &to in TaskStatus::ALL {
                let mut at = from;
                for step in path(from, to) {
                    assert!(at.can_transition_to(step), "{from} -> {to} via {step}");
                    at = step;
                }
            }
        }
    }
}
