//! The unit of work ([`Tx`]) and the **Repository** traits it implements.
//!
//! Every trait here is implemented by [`Tx`] and nothing else: the engine
//! composes calls across repositories inside one `Tx` and commits once, which
//! is how "step/run state + events + outbox commit together" holds.
//! Higher crates may write their own fakes against these traits for tests.

use std::str::FromStr;

use async_trait::async_trait;
use serde_json::Value;
use sqlx::sqlite::SqliteRow;
use sqlx::{Row, Sqlite, SqliteConnection, Transaction};
use sun_core::model::{
    ActivityKind, Budget, ContextPack, EventRecord, OutboxItem, OutboxStatus, Run, Step, Task,
    TaskActivity, TaskKey, TaskKind, Workflow, WorkflowVersion,
};
use sun_core::{
    AgentId, BlobHash, ContextPackId, MemberId, OutboxId, RunId, RunStatus, RuntimeId, SessionId,
    StateMachine, StepId, StepKey, StepStatus, SunEvent, TaskActivityId, TaskId, TaskStatus,
    Timestamp, WorkflowId, WorkspaceId,
};
use tokio::sync::broadcast;

use crate::agent::HIDDEN;
use crate::{MasterKey, Result, SecretRepo, StoreError};

/// The broadcasts a write [`Tx`] publishes to on commit (**Observer**).
#[derive(Debug, Clone)]
pub(crate) struct Channels {
    pub(crate) events: broadcast::Sender<EventRecord>,
    pub(crate) activity: broadcast::Sender<TaskActivity>,
}

/// One database transaction — the **Unit of Work**. Get one from
/// [`crate::Store::begin`] (write) or [`crate::Store::read`] (read-only snapshot).
///
/// - [`Tx::commit`] makes every write visible at once, then publishes the
///   appended events and task activity to [`crate::Store::subscribe`]rs and
///   [`crate::Store::subscribe_activity`]rs (**Observer**), so a subscriber
///   never sees a row that was rolled back.
/// - Dropping a `Tx` without committing rolls it back.
/// - Keep it short: no `.await` on anything but this `Tx` while it is open
///   (no network, no host calls, no blob I/O). Write transactions are
///   serialized, so a held `Tx` blocks every other writer.
pub struct Tx {
    inner: Transaction<'static, Sqlite>,
    appended: Vec<EventRecord>,
    activity: Vec<TaskActivity>,
    notify: Option<Channels>,
    key: MasterKey,
}

impl Tx {
    pub(crate) fn new(
        inner: Transaction<'static, Sqlite>,
        notify: Option<Channels>,
        key: MasterKey,
    ) -> Self {
        Self {
            inner,
            appended: Vec::new(),
            activity: Vec::new(),
            notify,
            key,
        }
    }

    pub(crate) fn key(&self) -> &MasterKey {
        &self.key
    }

    /// Commits, then broadcasts the events and activity appended in this transaction.
    pub async fn commit(self) -> Result<()> {
        self.inner.commit().await?;
        if let Some(notify) = self.notify {
            // No subscribers is fine; lagging ones catch up from the store.
            for event in self.appended {
                let _ = notify.events.send(event);
            }
            for activity in self.activity {
                let _ = notify.activity.send(activity);
            }
        }
        Ok(())
    }

    pub(crate) fn conn(&mut self) -> &mut SqliteConnection {
        &mut self.inner
    }
}

/// `workflow` and `workflow_version`.
#[async_trait]
pub trait WorkflowRepo {
    /// Inserts a workflow identity.
    async fn insert_workflow(&mut self, workflow: &Workflow) -> Result<()>;
    /// Renames a workflow (its latest version changed `name:`).
    async fn rename_workflow(&mut self, id: WorkflowId, name: &str) -> Result<()>;
    /// The workflow, or `NotFound`.
    async fn get_workflow(&mut self, id: WorkflowId) -> Result<Workflow>;
    /// Saves `source_yaml` as the next version (1, 2, ...).
    async fn save_workflow_version(
        &mut self,
        workflow_id: WorkflowId,
        source_yaml: &str,
        schema_hash: &str,
    ) -> Result<WorkflowVersion>;
    /// One version, or `NotFound`. Runs read their pinned version with this.
    async fn get_workflow_version(
        &mut self,
        workflow_id: WorkflowId,
        version: u32,
    ) -> Result<WorkflowVersion>;
    /// The newest version, if any was saved.
    async fn latest_workflow_version(
        &mut self,
        workflow_id: WorkflowId,
    ) -> Result<Option<WorkflowVersion>>;
    /// Every workflow, oldest first; hidden ones are left out
    /// (`AgentRepo::hidden_workflows`).
    async fn workflows(&mut self) -> Result<Vec<Workflow>>;
    /// The workflows of one workspace, oldest first, without hidden ones.
    async fn workflows_of_workspace(&mut self, workspace_id: WorkspaceId) -> Result<Vec<Workflow>>;
}

/// `task` and its `task_activity` log.
#[async_trait]
pub trait TaskRepo {
    /// Inserts a task with the next number of its workspace
    /// (`issue_counter + 1`, counted here, so numbers are unique) and that
    /// number as its position; returns the number. A chat takes no number
    /// (position 0). `task.number` and `task.position` are not read.
    /// `InvalidInput` when its workflow or project is on another workspace.
    async fn insert_task(&mut self, task: &Task) -> Result<Option<u32>>;
    /// The task, or `NotFound`.
    async fn get_task(&mut self, id: TaskId) -> Result<Task>;
    /// The task `key` names (its prefix in any case), or `NotFound`.
    async fn task_by_key(&mut self, key: &TaskKey) -> Result<Task>;
    /// Moves the task to `next` via [`StateMachine::transition`] and sets
    /// `updated_at`; returns the previous status. An illegal move is
    /// `InvalidTransition` and writes nothing. Append its activity too.
    async fn set_task_status(&mut self, id: TaskId, next: TaskStatus) -> Result<TaskStatus>;
    /// PR automation's move: the task goes to `to` (`done` or `needs_you`)
    /// along [`TaskStatus::can_move_by_merge`], and `updated_at` is set.
    /// Returns the previous status, or `None` (nothing written) when the
    /// task is not open. `NotFound` for a missing task. Append its activity too.
    async fn set_task_status_by_merge(
        &mut self,
        id: TaskId,
        to: TaskStatus,
    ) -> Result<Option<TaskStatus>>;
    /// Turns "move when PRs merge" off (or back on) for one task; `NotFound`
    /// when missing.
    async fn set_pr_auto_complete(&mut self, id: TaskId, disabled: bool) -> Result<()>;
    /// Every task (no chats), highest [`Priority::rank`] first, then oldest first.
    async fn tasks(&mut self) -> Result<Vec<Task>>;
    /// The tasks of one workspace, in [`Self::tasks`] order.
    async fn tasks_of_workspace(&mut self, workspace_id: WorkspaceId) -> Result<Vec<Task>>;
    /// The chat `agent` was @mentioned into on `source`, if any.
    async fn mention_chat(&mut self, source: TaskId, agent: AgentId) -> Result<Option<Task>>;
    /// The direct chats (no source task) `creator` started on `workspace`
    /// (`None`: the token holder's), last changed first.
    async fn chats_of(
        &mut self,
        workspace_id: WorkspaceId,
        creator: Option<MemberId>,
    ) -> Result<Vec<Task>>;
    /// Saves the task's title, body, workflow, priority, assignee, project
    /// and position, and sets `updated_at`. `NotFound` if there is no task;
    /// `InvalidInput` when the project is on another workspace.
    async fn update_task(&mut self, task: &Task) -> Result<()>;
    /// Appends an entry to the task's activity log with the next `seq`;
    /// its strings are redacted of stored secrets, and it is broadcast when
    /// the transaction commits
    /// ([`crate::Store::subscribe_activity`]).
    async fn append_activity(
        &mut self,
        task_id: TaskId,
        kind: ActivityKind,
        actor: Option<MemberId>,
        body: Value,
    ) -> Result<TaskActivity>;
    /// The task's activity with `seq > after_seq`, oldest first.
    async fn activity_of(&mut self, task_id: TaskId, after_seq: i64) -> Result<Vec<TaskActivity>>;
}

/// `run` and its `step` attempts.
#[async_trait]
pub trait RunRepo {
    /// Inserts a run (its workflow version must exist).
    async fn insert_run(&mut self, run: &Run) -> Result<()>;
    /// The run, or `NotFound`.
    async fn get_run(&mut self, id: RunId) -> Result<Run>;
    /// The workspace of the run's task, or `NotFound`.
    async fn workspace_of_run(&mut self, id: RunId) -> Result<WorkspaceId>;
    /// Every run in a non-terminal status, oldest first — what resume-on-boot loads.
    async fn active_runs(&mut self) -> Result<Vec<Run>>;
    /// Every run of a task, oldest first.
    async fn runs_of_task(&mut self, task_id: TaskId) -> Result<Vec<Run>>;
    /// Moves the run to `next` via [`StateMachine::transition`] and sets
    /// `end_reason`; returns the previous status.
    async fn set_run_status(
        &mut self,
        id: RunId,
        next: RunStatus,
        end_reason: Option<&str>,
    ) -> Result<RunStatus>;
    /// Pins the run to another saved version of its workflow, with that
    /// version's budget (a run starting on the repo's workflow file).
    async fn repin_run(&mut self, id: RunId, workflow_version: u32, budget: &Budget) -> Result<()>;

    /// Inserts a step attempt. `(run_id, step_key, attempt)` is unique.
    async fn insert_step(&mut self, step: &Step) -> Result<()>;
    /// The step attempt, or `NotFound`.
    async fn get_step(&mut self, id: StepId) -> Result<Step>;
    /// All attempts of a run, by step key then attempt.
    async fn steps_of_run(&mut self, run_id: RunId) -> Result<Vec<Step>>;
    /// Moves the step to `next` via [`StateMachine::transition`]; returns the
    /// previous status. Sets `started_at` on the first move to `running` and
    /// `ended_at` on a terminal status.
    async fn set_step_status(&mut self, id: StepId, next: StepStatus) -> Result<StepStatus>;
    /// Records the runtime and session an agent step was dispatched to.
    async fn assign_step_session(
        &mut self,
        id: StepId,
        runtime_id: RuntimeId,
        session_id: SessionId,
    ) -> Result<()>;
}

/// An event to append; the store assigns `seq`.
#[derive(Debug, Clone, PartialEq)]
pub struct NewEvent {
    /// The run.
    pub run_id: RunId,
    /// The step, for step-level events.
    pub step_key: Option<StepKey>,
    /// Event kind, e.g. `tool_call` or `step_status`.
    pub kind: String,
    /// Inline payload. Producers redact it; the store also redacts every
    /// string in it on append, with all stored secret values.
    pub payload: Value,
    /// Payload too big to inline, put in the blob store first.
    pub blob: Option<BlobHash>,
    /// When it happened.
    pub at: Timestamp,
}

impl NewEvent {
    /// An event of `kind` happening now.
    pub fn new(run_id: RunId, step_key: Option<StepKey>, kind: &str, payload: Value) -> Self {
        Self {
            run_id,
            step_key,
            kind: kind.to_owned(),
            payload,
            blob: None,
            at: chrono::Utc::now(),
        }
    }

    /// A normalized agent event; `kind` is the [`SunEvent`] tag and the
    /// payload is its JSON form. Text must already be redacted.
    pub fn agent(run_id: RunId, step_key: StepKey, event: &SunEvent) -> Result<Self> {
        let payload = serde_json::to_value(event)?;
        let kind = payload
            .get("kind")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        Ok(Self::new(run_id, Some(step_key), &kind, payload))
    }
}

/// The append-only `event` log (**Event sourcing**). Subscribe with
/// [`crate::Store::subscribe`].
#[async_trait]
pub trait EventLog {
    /// Appends an event; it is broadcast when the transaction commits.
    async fn append_event(&mut self, event: NewEvent) -> Result<EventRecord>;
    /// Appends an event a host sent as `(session_id, host_seq)`. A redelivered
    /// pair returns `None` and writes nothing (exactly-once).
    async fn append_host_event(
        &mut self,
        session_id: SessionId,
        host_seq: i64,
        event: NewEvent,
    ) -> Result<Option<EventRecord>>;
    /// Up to `limit` events with `seq > after_seq`, ascending; only `run_id`'s
    /// when given. Pass the last `seq` you saw to page or to catch up.
    async fn events_after(
        &mut self,
        run_id: Option<RunId>,
        after_seq: i64,
        limit: u32,
    ) -> Result<Vec<EventRecord>>;
    /// Sum of the `usage` events, of `run_id` only when given (cost).
    async fn usage(&mut self, run_id: Option<RunId>) -> Result<Usage>;
    /// How many events of `kind` every run has.
    async fn count_events(&mut self, kind: &str) -> Result<i64>;
    /// Every event of `kind`, of every run, oldest first.
    async fn events_of_kind(&mut self, kind: &str) -> Result<Vec<EventRecord>>;
}

/// Summed token and cost usage (`usage` agent events).
#[derive(Debug, Clone, Copy, Default, PartialEq, serde::Serialize)]
pub struct Usage {
    /// Input tokens.
    pub input_tokens: i64,
    /// Output tokens.
    pub output_tokens: i64,
    /// Cost in USD of the events that reported one.
    pub cost_usd: f64,
}

/// The `outbox` (**Transactional outbox**). Enqueue in the same `Tx` as the
/// state change; a worker delivers after commit.
#[async_trait]
pub trait OutboxRepo {
    /// Inserts the item unless its `idem_key` exists. Returns `false` for a
    /// duplicate: the earlier row stands, so a retry never doubles a side effect.
    async fn enqueue(&mut self, item: &OutboxItem) -> Result<bool>;
    /// Up to `limit` items in `status`, oldest first.
    async fn outbox_with_status(
        &mut self,
        status: OutboxStatus,
        limit: u32,
    ) -> Result<Vec<OutboxItem>>;
    /// Records a delivery outcome.
    async fn set_outbox_status(
        &mut self,
        id: OutboxId,
        status: OutboxStatus,
        result: Option<&Value>,
    ) -> Result<()>;
}

#[async_trait]
impl WorkflowRepo for Tx {
    async fn insert_workflow(&mut self, workflow: &Workflow) -> Result<()> {
        sqlx::query("INSERT INTO workflow (id, name, workspace_id) VALUES ($1, $2, $3)")
            .bind(workflow.id.to_string())
            .bind(&workflow.name)
            .bind(workflow.workspace_id.to_string())
            .execute(self.conn())
            .await?;
        Ok(())
    }

    async fn rename_workflow(&mut self, id: WorkflowId, name: &str) -> Result<()> {
        sqlx::query("UPDATE workflow SET name = $1 WHERE id = $2")
            .bind(name)
            .bind(id.to_string())
            .execute(self.conn())
            .await?;
        Ok(())
    }

    async fn get_workflow(&mut self, id: WorkflowId) -> Result<Workflow> {
        let row = sqlx::query("SELECT id, name, workspace_id FROM workflow WHERE id = $1")
            .bind(id.to_string())
            .fetch_optional(self.conn())
            .await?
            .ok_or_else(|| StoreError::not_found("workflow", id))?;
        workflow_from(&row)
    }

    async fn save_workflow_version(
        &mut self,
        workflow_id: WorkflowId,
        source_yaml: &str,
        schema_hash: &str,
    ) -> Result<WorkflowVersion> {
        let row = sqlx::query(
            "INSERT INTO workflow_version (workflow_id, version, source_yaml, schema_hash)
             SELECT $1, COALESCE(MAX(version), 0) + 1, $2, $3
             FROM workflow_version WHERE workflow_id = $1
             RETURNING workflow_id, version, source_yaml, schema_hash",
        )
        .bind(workflow_id.to_string())
        .bind(source_yaml)
        .bind(schema_hash)
        .fetch_one(self.conn())
        .await?;
        workflow_version_from(&row)
    }

    async fn get_workflow_version(
        &mut self,
        workflow_id: WorkflowId,
        version: u32,
    ) -> Result<WorkflowVersion> {
        let row = sqlx::query(
            "SELECT workflow_id, version, source_yaml, schema_hash
             FROM workflow_version WHERE workflow_id = $1 AND version = $2",
        )
        .bind(workflow_id.to_string())
        .bind(i64::from(version))
        .fetch_optional(self.conn())
        .await?
        .ok_or_else(|| {
            StoreError::not_found("workflow version", format!("{workflow_id}@{version}"))
        })?;
        workflow_version_from(&row)
    }

    async fn latest_workflow_version(
        &mut self,
        workflow_id: WorkflowId,
    ) -> Result<Option<WorkflowVersion>> {
        sqlx::query(
            "SELECT workflow_id, version, source_yaml, schema_hash
             FROM workflow_version WHERE workflow_id = $1 ORDER BY version DESC LIMIT 1",
        )
        .bind(workflow_id.to_string())
        .fetch_optional(self.conn())
        .await?
        .as_ref()
        .map(workflow_version_from)
        .transpose()
    }

    async fn workflows(&mut self) -> Result<Vec<Workflow>> {
        sqlx::query(&format!(
            "SELECT id, name, workspace_id FROM workflow WHERE NOT {HIDDEN} ORDER BY id"
        ))
        .fetch_all(self.conn())
        .await?
        .iter()
        .map(workflow_from)
        .collect()
    }

    async fn workflows_of_workspace(&mut self, workspace_id: WorkspaceId) -> Result<Vec<Workflow>> {
        sqlx::query(&format!(
            "SELECT id, name, workspace_id FROM workflow
             WHERE workspace_id = $1 AND NOT {HIDDEN} ORDER BY id"
        ))
        .bind(workspace_id.to_string())
        .fetch_all(self.conn())
        .await?
        .iter()
        .map(workflow_from)
        .collect()
    }
}

pub(crate) fn workflow_from(row: &SqliteRow) -> Result<Workflow> {
    Ok(Workflow {
        id: parse(row.try_get("id")?)?,
        name: row.try_get("name")?,
        workspace_id: parse(row.try_get("workspace_id")?)?,
    })
}

const TASK_COLUMNS: &str = "id, workspace_id, number, title, body, workflow_id, priority, status, \
     pr_auto_complete_disabled, assignee_agent_id, project_id, position, created_by, created_at, \
     updated_at, kind, source_task_id";

impl Tx {
    /// `InvalidInput` unless the task's workflow and project are on its workspace.
    async fn check_task_links(&mut self, task: &Task) -> Result<()> {
        let elsewhere = |what: &str| {
            Err(sun_core::Error::InvalidInput(format!(
                "The {what} is on another workspace than the task."
            ))
            .into())
        };
        if let Some(workflow) = task.workflow_id
            && self.get_workflow(workflow).await?.workspace_id != task.workspace_id
        {
            return elsewhere("workflow");
        }
        if let Some(project) = task.project_id
            && crate::ProjectRepo::get_project(self, project)
                .await?
                .workspace_id
                != task.workspace_id
        {
            return elsewhere("project");
        }
        Ok(())
    }

    /// The tasks a `WHERE` clause picks, highest priority rank first, then oldest.
    async fn tasks_where(&mut self, filter: &str, bind: Option<String>) -> Result<Vec<Task>> {
        let sql = format!("SELECT {TASK_COLUMNS} FROM task {filter} ORDER BY id");
        let query = match bind {
            Some(value) => sqlx::query(&sql).bind(value),
            None => sqlx::query(&sql),
        };
        let mut tasks = query
            .fetch_all(self.conn())
            .await?
            .iter()
            .map(task_from)
            .collect::<Result<Vec<_>>>()?;
        // Stable: equal ranks keep id (creation) order.
        tasks.sort_by_key(|t| std::cmp::Reverse(t.priority.rank()));
        Ok(tasks)
    }
}

#[async_trait]
impl TaskRepo for Tx {
    async fn insert_task(&mut self, task: &Task) -> Result<Option<u32>> {
        self.check_task_links(task).await?;
        let number: Option<i64> = match task.kind {
            TaskKind::Chat => None,
            TaskKind::Task => Some(
                sqlx::query_scalar(
                    "UPDATE workspace SET issue_counter = issue_counter + 1 WHERE id = $1
                     RETURNING issue_counter",
                )
                .bind(task.workspace_id.to_string())
                .fetch_optional(self.conn())
                .await?
                .ok_or_else(|| StoreError::not_found("workspace", task.workspace_id))?,
            ),
        };
        sqlx::query(&format!(
            "INSERT INTO task ({TASK_COLUMNS})
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, coalesce($3, 0), $12, $13,
                     $14, $15, $16)"
        ))
        .bind(task.id.to_string())
        .bind(task.workspace_id.to_string())
        .bind(number)
        .bind(&task.title)
        .bind(&task.body)
        .bind(task.workflow_id.map(|w| w.to_string()))
        .bind(task.priority.as_str())
        .bind(task.status.as_str())
        .bind(task.pr_auto_complete_disabled)
        .bind(task.assignee_agent_id.map(|a| a.to_string()))
        .bind(task.project_id.map(|p| p.to_string()))
        .bind(task.created_by.map(|m| m.to_string()))
        .bind(ts(&task.created_at))
        .bind(ts(&task.updated_at))
        .bind(task.kind.as_str())
        .bind(task.source_task_id.map(|t| t.to_string()))
        .execute(self.conn())
        .await?;
        number
            .map(|n| {
                u32::try_from(n)
                    .map_err(|_| sun_core::Error::InvalidInput(format!("task number {n}")).into())
            })
            .transpose()
    }

    async fn get_task(&mut self, id: TaskId) -> Result<Task> {
        let row = sqlx::query(&format!("SELECT {TASK_COLUMNS} FROM task WHERE id = $1"))
            .bind(id.to_string())
            .fetch_optional(self.conn())
            .await?
            .ok_or_else(|| StoreError::not_found("task", id))?;
        task_from(&row)
    }

    async fn task_by_key(&mut self, key: &TaskKey) -> Result<Task> {
        let row = sqlx::query(&format!(
            "SELECT {TASK_COLUMNS} FROM task WHERE number = $2 AND workspace_id =
             (SELECT id FROM workspace WHERE upper(issue_prefix) = upper($1))"
        ))
        .bind(&key.prefix)
        .bind(i64::from(key.number))
        .fetch_optional(self.conn())
        .await?
        .ok_or_else(|| StoreError::not_found("task", key))?;
        task_from(&row)
    }

    async fn set_task_status(&mut self, id: TaskId, next: TaskStatus) -> Result<TaskStatus> {
        let previous = transition(self.conn(), "task", &id.to_string(), next).await?;
        sqlx::query("UPDATE task SET updated_at = $1 WHERE id = $2")
            .bind(ts(&chrono::Utc::now()))
            .bind(id.to_string())
            .execute(self.conn())
            .await?;
        Ok(previous)
    }

    async fn set_task_status_by_merge(
        &mut self,
        id: TaskId,
        to: TaskStatus,
    ) -> Result<Option<TaskStatus>> {
        let current: TaskStatus = parse(
            sqlx::query_scalar("SELECT status FROM task WHERE id = $1")
                .bind(id.to_string())
                .fetch_optional(self.conn())
                .await?
                .ok_or_else(|| StoreError::not_found("task", id))?,
        )?;
        if !current.can_move_by_merge(to) {
            return Ok(None);
        }
        sqlx::query("UPDATE task SET status = $1, updated_at = $2 WHERE id = $3")
            .bind(to.as_str())
            .bind(ts(&chrono::Utc::now()))
            .bind(id.to_string())
            .execute(self.conn())
            .await?;
        Ok(Some(current))
    }

    async fn set_pr_auto_complete(&mut self, id: TaskId, disabled: bool) -> Result<()> {
        let done = sqlx::query("UPDATE task SET pr_auto_complete_disabled = $1 WHERE id = $2")
            .bind(disabled)
            .bind(id.to_string())
            .execute(self.conn())
            .await?;
        if done.rows_affected() == 0 {
            return Err(StoreError::not_found("task", id));
        }
        Ok(())
    }

    async fn tasks(&mut self) -> Result<Vec<Task>> {
        self.tasks_where("WHERE kind = 'task'", None).await
    }

    async fn tasks_of_workspace(&mut self, workspace_id: WorkspaceId) -> Result<Vec<Task>> {
        let filter = "WHERE kind = 'task' AND workspace_id = $1";
        self.tasks_where(filter, Some(workspace_id.to_string()))
            .await
    }

    async fn mention_chat(&mut self, source: TaskId, agent: AgentId) -> Result<Option<Task>> {
        let row = sqlx::query(&format!(
            "SELECT {TASK_COLUMNS} FROM task WHERE source_task_id = $1 AND assignee_agent_id = $2"
        ))
        .bind(source.to_string())
        .bind(agent.to_string())
        .fetch_optional(self.conn())
        .await?;
        row.as_ref().map(task_from).transpose()
    }

    async fn chats_of(
        &mut self,
        workspace_id: WorkspaceId,
        creator: Option<MemberId>,
    ) -> Result<Vec<Task>> {
        sqlx::query(&format!(
            "SELECT {TASK_COLUMNS} FROM task
             WHERE kind = 'chat' AND source_task_id IS NULL AND workspace_id = $1
               AND created_by IS $2
             ORDER BY updated_at DESC, id DESC"
        ))
        .bind(workspace_id.to_string())
        .bind(creator.map(|m| m.to_string()))
        .fetch_all(self.conn())
        .await?
        .iter()
        .map(task_from)
        .collect()
    }

    async fn update_task(&mut self, task: &Task) -> Result<()> {
        self.check_task_links(task).await?;
        let done = sqlx::query(
            "UPDATE task SET title = $1, body = $2, priority = $3, assignee_agent_id = $4,
               project_id = $5, position = $6, updated_at = $7, workflow_id = $9
             WHERE id = $8",
        )
        .bind(&task.title)
        .bind(&task.body)
        .bind(task.priority.as_str())
        .bind(task.assignee_agent_id.map(|a| a.to_string()))
        .bind(task.project_id.map(|p| p.to_string()))
        .bind(task.position)
        .bind(ts(&chrono::Utc::now()))
        .bind(task.id.to_string())
        .bind(task.workflow_id.map(|w| w.to_string()))
        .execute(self.conn())
        .await?;
        if done.rows_affected() == 0 {
            return Err(StoreError::not_found("task", task.id));
        }
        Ok(())
    }

    async fn append_activity(
        &mut self,
        task_id: TaskId,
        kind: ActivityKind,
        actor: Option<MemberId>,
        mut body: Value,
    ) -> Result<TaskActivity> {
        self.redact_known(&mut body).await?;
        let row = sqlx::query(
            "INSERT INTO task_activity (id, task_id, seq, kind, actor_member_id, body_json, created_at)
             SELECT $1, $2, COALESCE(MAX(seq), 0) + 1, $3, $4, $5, $6
             FROM task_activity WHERE task_id = $2
             RETURNING id, task_id, seq, kind, actor_member_id, body_json, created_at",
        )
        .bind(TaskActivityId::generate().to_string())
        .bind(task_id.to_string())
        .bind(kind.as_str())
        .bind(actor.map(|m| m.to_string()))
        .bind(serde_json::to_string(&body)?)
        .bind(ts(&chrono::Utc::now()))
        .fetch_one(self.conn())
        .await?;
        let activity = activity_from(&row)?;
        self.activity.push(activity.clone());
        Ok(activity)
    }

    async fn activity_of(&mut self, task_id: TaskId, after_seq: i64) -> Result<Vec<TaskActivity>> {
        sqlx::query(
            "SELECT id, task_id, seq, kind, actor_member_id, body_json, created_at
             FROM task_activity WHERE task_id = $1 AND seq > $2 ORDER BY seq",
        )
        .bind(task_id.to_string())
        .bind(after_seq)
        .fetch_all(self.conn())
        .await?
        .iter()
        .map(activity_from)
        .collect()
    }
}

const RUN_COLUMNS: &str =
    "id, task_id, workflow_id, workflow_version, status, end_reason, budget_json, repo_ids_json";
const STEP_COLUMNS: &str =
    "id, run_id, step_key, attempt, status, runtime_id, session_id, started_at, ended_at";

#[async_trait]
impl RunRepo for Tx {
    async fn insert_run(&mut self, run: &Run) -> Result<()> {
        sqlx::query(&format!(
            "INSERT INTO run ({RUN_COLUMNS}) VALUES ($1, $2, $3, $4, $5, $6, $7, $8)"
        ))
        .bind(run.id.to_string())
        .bind(run.task_id.to_string())
        .bind(run.workflow_id.to_string())
        .bind(i64::from(run.workflow_version))
        .bind(run.status.as_str())
        .bind(&run.end_reason)
        .bind(serde_json::to_string(&run.budget)?)
        .bind(serde_json::to_string(&run.repo_ids)?)
        .execute(self.conn())
        .await?;
        Ok(())
    }

    async fn get_run(&mut self, id: RunId) -> Result<Run> {
        let row = sqlx::query(&format!("SELECT {RUN_COLUMNS} FROM run WHERE id = $1"))
            .bind(id.to_string())
            .fetch_optional(self.conn())
            .await?
            .ok_or_else(|| StoreError::not_found("run", id))?;
        run_from(&row)
    }

    async fn workspace_of_run(&mut self, id: RunId) -> Result<WorkspaceId> {
        let workspace: String = sqlx::query_scalar(
            "SELECT t.workspace_id FROM run r JOIN task t ON t.id = r.task_id WHERE r.id = $1",
        )
        .bind(id.to_string())
        .fetch_optional(self.conn())
        .await?
        .ok_or_else(|| StoreError::not_found("run", id))?;
        parse(workspace)
    }

    async fn active_runs(&mut self) -> Result<Vec<Run>> {
        let active: Vec<&str> = RunStatus::ALL
            .iter()
            .filter(|status| !status.is_terminal())
            .map(|status| status.as_str())
            .collect();
        sqlx::query(&format!(
            "SELECT {RUN_COLUMNS} FROM run
             WHERE status IN (SELECT value FROM json_each($1)) ORDER BY id"
        ))
        .bind(serde_json::to_string(&active)?)
        .fetch_all(self.conn())
        .await?
        .iter()
        .map(run_from)
        .collect()
    }

    async fn runs_of_task(&mut self, task_id: TaskId) -> Result<Vec<Run>> {
        sqlx::query(&format!(
            "SELECT {RUN_COLUMNS} FROM run WHERE task_id = $1 ORDER BY id"
        ))
        .bind(task_id.to_string())
        .fetch_all(self.conn())
        .await?
        .iter()
        .map(run_from)
        .collect()
    }

    async fn set_run_status(
        &mut self,
        id: RunId,
        next: RunStatus,
        end_reason: Option<&str>,
    ) -> Result<RunStatus> {
        let previous = transition(self.conn(), "run", &id.to_string(), next).await?;
        sqlx::query("UPDATE run SET end_reason = $1 WHERE id = $2")
            .bind(end_reason)
            .bind(id.to_string())
            .execute(self.conn())
            .await?;
        Ok(previous)
    }

    async fn repin_run(&mut self, id: RunId, workflow_version: u32, budget: &Budget) -> Result<()> {
        sqlx::query("UPDATE run SET workflow_version = $1, budget_json = $2 WHERE id = $3")
            .bind(i64::from(workflow_version))
            .bind(serde_json::to_string(budget)?)
            .bind(id.to_string())
            .execute(self.conn())
            .await?;
        Ok(())
    }

    async fn insert_step(&mut self, step: &Step) -> Result<()> {
        sqlx::query(&format!(
            "INSERT INTO step ({STEP_COLUMNS}) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)"
        ))
        .bind(step.id.to_string())
        .bind(step.run_id.to_string())
        .bind(step.step_key.as_str())
        .bind(i64::from(step.attempt))
        .bind(step.status.as_str())
        .bind(step.runtime_id.map(|id| id.to_string()))
        .bind(step.session_id.map(|id| id.to_string()))
        .bind(step.started_at.as_ref().map(ts))
        .bind(step.ended_at.as_ref().map(ts))
        .execute(self.conn())
        .await?;
        Ok(())
    }

    async fn get_step(&mut self, id: StepId) -> Result<Step> {
        let row = sqlx::query(&format!("SELECT {STEP_COLUMNS} FROM step WHERE id = $1"))
            .bind(id.to_string())
            .fetch_optional(self.conn())
            .await?
            .ok_or_else(|| StoreError::not_found("step", id))?;
        step_from(&row)
    }

    async fn steps_of_run(&mut self, run_id: RunId) -> Result<Vec<Step>> {
        sqlx::query(&format!(
            "SELECT {STEP_COLUMNS} FROM step WHERE run_id = $1 ORDER BY step_key, attempt"
        ))
        .bind(run_id.to_string())
        .fetch_all(self.conn())
        .await?
        .iter()
        .map(step_from)
        .collect()
    }

    async fn set_step_status(&mut self, id: StepId, next: StepStatus) -> Result<StepStatus> {
        let previous = transition(self.conn(), "step", &id.to_string(), next).await?;
        let now = chrono::Utc::now();
        sqlx::query(
            "UPDATE step SET
               started_at = CASE WHEN $1 AND started_at IS NULL THEN $3 ELSE started_at END,
               ended_at   = CASE WHEN $2 THEN $3 ELSE ended_at END
             WHERE id = $4",
        )
        .bind(next == StepStatus::Running)
        .bind(next.is_terminal())
        .bind(ts(&now))
        .bind(id.to_string())
        .execute(self.conn())
        .await?;
        Ok(previous)
    }

    async fn assign_step_session(
        &mut self,
        id: StepId,
        runtime_id: RuntimeId,
        session_id: SessionId,
    ) -> Result<()> {
        let done = sqlx::query("UPDATE step SET runtime_id = $1, session_id = $2 WHERE id = $3")
            .bind(runtime_id.to_string())
            .bind(session_id.to_string())
            .bind(id.to_string())
            .execute(self.conn())
            .await?;
        if done.rows_affected() == 0 {
            return Err(StoreError::not_found("step", id));
        }
        Ok(())
    }
}

pub(crate) const EVENT_COLUMNS: &str = "seq, run_id, step_key, kind, payload_json, blob, at";

impl Tx {
    /// Last line of defence for the ledgers (run events, task activity): no
    /// stored secret value is written, even if the producer forgot to redact.
    // ponytail: decrypts every secret per append; cache the values in
    // `Store` (invalidated by `SecretRepo` writes) if appends profile hot.
    async fn redact_known(&mut self, value: &mut Value) -> Result<()> {
        let known = self.known_secrets().await?;
        let known: Vec<&str> = known.iter().map(String::as_str).collect();
        redact_json(value, &known);
        Ok(())
    }

    async fn insert_event(
        &mut self,
        host: Option<(SessionId, i64)>,
        mut event: NewEvent,
    ) -> Result<EventRecord> {
        self.redact_known(&mut event.payload).await?;
        let row = sqlx::query(&format!(
            "INSERT INTO event (run_id, step_key, kind, payload_json, blob, at, session_id, host_seq)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8)
             RETURNING {EVENT_COLUMNS}"
        ))
        .bind(event.run_id.to_string())
        .bind(event.step_key.as_ref().map(StepKey::as_str))
        .bind(&event.kind)
        .bind(serde_json::to_string(&event.payload)?)
        .bind(event.blob.as_ref().map(BlobHash::as_str))
        .bind(ts(&event.at))
        .bind(host.map(|(session, _)| session.to_string()))
        .bind(host.map(|(_, seq)| seq))
        .fetch_one(self.conn())
        .await?;
        let record = event_from(&row)?;
        self.appended.push(record.clone());
        Ok(record)
    }
}

#[async_trait]
impl EventLog for Tx {
    async fn append_event(&mut self, event: NewEvent) -> Result<EventRecord> {
        self.insert_event(None, event).await
    }

    async fn append_host_event(
        &mut self,
        session_id: SessionId,
        host_seq: i64,
        event: NewEvent,
    ) -> Result<Option<EventRecord>> {
        // Check first instead of `ON CONFLICT DO NOTHING`: a skipped
        // AUTOINCREMENT insert still burns a `seq` and would leave a gap.
        // Writes are serialized, so check-then-insert cannot race; the unique
        // index stays as the backstop.
        let seen = sqlx::query("SELECT 1 FROM event WHERE session_id = $1 AND host_seq = $2")
            .bind(session_id.to_string())
            .bind(host_seq)
            .fetch_optional(self.conn())
            .await?
            .is_some();
        if seen {
            return Ok(None);
        }
        Ok(Some(
            self.insert_event(Some((session_id, host_seq)), event)
                .await?,
        ))
    }

    async fn events_after(
        &mut self,
        run_id: Option<RunId>,
        after_seq: i64,
        limit: u32,
    ) -> Result<Vec<EventRecord>> {
        // Two statements so the per-run timeline uses the (run_id, seq) index.
        let by_run = format!(
            "SELECT {EVENT_COLUMNS} FROM event WHERE run_id = $1 AND seq > $2 ORDER BY seq LIMIT $3"
        );
        let all = format!("SELECT {EVENT_COLUMNS} FROM event WHERE seq > $1 ORDER BY seq LIMIT $2");
        let query = match run_id {
            Some(run_id) => sqlx::query(&by_run).bind(run_id.to_string()),
            None => sqlx::query(&all),
        };
        query
            .bind(after_seq)
            .bind(i64::from(limit))
            .fetch_all(self.conn())
            .await?
            .iter()
            .map(event_from)
            .collect()
    }

    async fn events_of_kind(&mut self, kind: &str) -> Result<Vec<EventRecord>> {
        sqlx::query(&format!(
            "SELECT {EVENT_COLUMNS} FROM event WHERE kind = $1 ORDER BY seq"
        ))
        .bind(kind)
        .fetch_all(self.conn())
        .await?
        .iter()
        .map(event_from)
        .collect()
    }

    async fn count_events(&mut self, kind: &str) -> Result<i64> {
        Ok(
            sqlx::query_scalar("SELECT COUNT(*) FROM event WHERE kind = $1")
                .bind(kind)
                .fetch_one(self.conn())
                .await?,
        )
    }

    async fn usage(&mut self, run_id: Option<RunId>) -> Result<Usage> {
        let row = sqlx::query(
            "SELECT CAST(COALESCE(SUM(CAST(json_extract(payload_json, '$.input') AS BIGINT)), 0)
                         AS BIGINT) AS input,
                    CAST(COALESCE(SUM(CAST(json_extract(payload_json, '$.output') AS BIGINT)), 0)
                         AS BIGINT) AS output,
                    CAST(COALESCE(SUM(CAST(json_extract(payload_json, '$.cost_usd')
                                  AS DOUBLE PRECISION)), 0) AS DOUBLE PRECISION) AS cost
             FROM event WHERE kind = 'usage' AND ($1 IS NULL OR run_id = $1)",
        )
        .bind(run_id.map(|id| id.to_string()))
        .fetch_one(self.conn())
        .await?;
        Ok(Usage {
            input_tokens: row.try_get("input")?,
            output_tokens: row.try_get("output")?,
            cost_usd: row.try_get("cost")?,
        })
    }
}

const OUTBOX_COLUMNS: &str = "id, run_id, idem_key, target, payload_json, status, result_json";

#[async_trait]
impl OutboxRepo for Tx {
    async fn enqueue(&mut self, item: &OutboxItem) -> Result<bool> {
        let done = sqlx::query(&format!(
            "INSERT INTO outbox ({OUTBOX_COLUMNS}) VALUES ($1, $2, $3, $4, $5, $6, $7)
             ON CONFLICT (idem_key) DO NOTHING"
        ))
        .bind(item.id.to_string())
        .bind(item.run_id.map(|r| r.to_string()))
        .bind(item.idem_key.as_str())
        .bind(&item.target)
        .bind(serde_json::to_string(&item.payload)?)
        .bind(item.status.as_str())
        .bind(
            item.result
                .as_ref()
                .map(serde_json::to_string)
                .transpose()?,
        )
        .execute(self.conn())
        .await?;
        Ok(done.rows_affected() == 1)
    }

    async fn outbox_with_status(
        &mut self,
        status: OutboxStatus,
        limit: u32,
    ) -> Result<Vec<OutboxItem>> {
        sqlx::query(&format!(
            "SELECT {OUTBOX_COLUMNS} FROM outbox WHERE status = $1 ORDER BY id LIMIT $2"
        ))
        .bind(status.as_str())
        .bind(i64::from(limit))
        .fetch_all(self.conn())
        .await?
        .iter()
        .map(outbox_from)
        .collect()
    }

    async fn set_outbox_status(
        &mut self,
        id: OutboxId,
        status: OutboxStatus,
        result: Option<&Value>,
    ) -> Result<()> {
        let done = sqlx::query("UPDATE outbox SET status = $1, result_json = $2 WHERE id = $3")
            .bind(status.as_str())
            .bind(result.map(serde_json::to_string).transpose()?)
            .bind(id.to_string())
            .execute(self.conn())
            .await?;
        if done.rows_affected() == 0 {
            return Err(StoreError::not_found("outbox item", id));
        }
        Ok(())
    }
}

/// `context_pack` — what each agent step was given. The pack body
/// is a blob; the row holds its hash, token estimate and item list.
#[async_trait]
pub trait ContextRepo {
    /// Inserts a stored pack.
    async fn insert_context_pack(&mut self, pack: &ContextPack) -> Result<()>;
    /// One pack, or `NotFound`.
    async fn get_context_pack(&mut self, id: ContextPackId) -> Result<ContextPack>;
}

#[async_trait]
impl ContextRepo for Tx {
    async fn insert_context_pack(&mut self, pack: &ContextPack) -> Result<()> {
        sqlx::query(
            "INSERT INTO context_pack (id, step_id, pack_hash, token_estimate, items_json) \
             VALUES ($1, $2, $3, $4, $5)",
        )
        .bind(pack.id.to_string())
        .bind(pack.step_id.to_string())
        .bind(pack.pack_hash.as_str())
        .bind(i64::from(pack.token_estimate))
        .bind(serde_json::to_string(&pack.items)?)
        .execute(self.conn())
        .await?;
        Ok(())
    }

    async fn get_context_pack(&mut self, id: ContextPackId) -> Result<ContextPack> {
        let row = sqlx::query(
            "SELECT id, step_id, pack_hash, token_estimate, items_json \
             FROM context_pack WHERE id = $1",
        )
        .bind(id.to_string())
        .fetch_optional(self.conn())
        .await?
        .ok_or_else(|| StoreError::not_found("context_pack", id))?;
        Ok(ContextPack {
            id: parse(row.try_get("id")?)?,
            step_id: parse(row.try_get("step_id")?)?,
            pack_hash: BlobHash::new(row.try_get::<String, _>("pack_hash")?)?,
            token_estimate: u32_at(&row, "token_estimate")?,
            items: json(row.try_get("items_json")?)?,
        })
    }
}

/// Reads `table.status` of row `id`, checks `current -> next` with the
/// state machine and writes it. The one place status columns change.
async fn transition<S>(
    conn: &mut SqliteConnection,
    table: &'static str,
    id: &str,
    next: S,
) -> Result<S>
where
    S: StateMachine + FromStr<Err = sun_core::Error>,
{
    let current: String = sqlx::query_scalar(&format!("SELECT status FROM {table} WHERE id = $1"))
        .bind(id)
        .fetch_optional(&mut *conn)
        .await?
        .ok_or_else(|| StoreError::not_found(table, id))?;
    let current: S = parse(current)?;
    current.transition(next)?;
    sqlx::query(&format!("UPDATE {table} SET status = $1 WHERE id = $2"))
        .bind(next.as_str())
        .bind(id)
        .execute(&mut *conn)
        .await?;
    Ok(current)
}

/// Runs every string in `value` through [`sun_core::redact::redact`].
fn redact_json(value: &mut Value, known: &[&str]) {
    match value {
        Value::String(text) => *text = sun_core::redact::redact(text, known),
        Value::Array(items) => items.iter_mut().for_each(|v| redact_json(v, known)),
        Value::Object(map) => map.values_mut().for_each(|v| redact_json(v, known)),
        _ => {}
    }
}

pub(crate) fn parse<T: FromStr<Err = sun_core::Error>>(text: String) -> Result<T> {
    Ok(text.parse()?)
}

/// A timestamp as stored: RFC 3339 text, the format sqlx's SQLite chrono
/// encoding always used, so old and new rows compare and sort alike.
pub(crate) fn ts(at: &Timestamp) -> String {
    at.to_rfc3339_opts(chrono::SecondsFormat::AutoSi, false)
}

/// Parses a stored timestamp (see [`ts`]).
pub(crate) fn parse_ts(text: String) -> Result<Timestamp> {
    chrono::DateTime::parse_from_rfc3339(&text)
        .map(|at| at.to_utc())
        .map_err(|_| sun_core::Error::InvalidInput(format!("stored timestamp {text:?}")).into())
}

/// A non-negative integer column as `u32`; a negative stored value is an error.
pub(crate) fn u32_at(row: &SqliteRow, column: &str) -> Result<u32> {
    let value: i64 = row.try_get(column)?;
    u32::try_from(value)
        .map_err(|_| sun_core::Error::InvalidInput(format!("stored {column} {value}")).into())
}

pub(crate) fn parse_ts_opt(text: Option<String>) -> Result<Option<Timestamp>> {
    text.map(parse_ts).transpose()
}

pub(crate) fn parse_opt<T: FromStr<Err = sun_core::Error>>(
    text: Option<String>,
) -> Result<Option<T>> {
    text.map(parse).transpose()
}

pub(crate) fn json<T: serde::de::DeserializeOwned>(text: String) -> Result<T> {
    Ok(serde_json::from_str(&text)?)
}

fn workflow_version_from(row: &SqliteRow) -> Result<WorkflowVersion> {
    Ok(WorkflowVersion {
        workflow_id: parse(row.try_get("workflow_id")?)?,
        version: u32_at(row, "version")?,
        source_yaml: row.try_get("source_yaml")?,
        schema_hash: row.try_get("schema_hash")?,
    })
}

fn task_from(row: &SqliteRow) -> Result<Task> {
    Ok(Task {
        id: parse(row.try_get("id")?)?,
        workspace_id: parse(row.try_get("workspace_id")?)?,
        number: row
            .try_get::<Option<i64>, _>("number")?
            .map(|n| {
                u32::try_from(n)
                    .map_err(|_| sun_core::Error::InvalidInput(format!("stored number {n}")))
            })
            .transpose()?,
        title: row.try_get("title")?,
        body: row.try_get("body")?,
        workflow_id: parse_opt(row.try_get("workflow_id")?)?,
        priority: parse(row.try_get("priority")?)?,
        status: parse(row.try_get("status")?)?,
        pr_auto_complete_disabled: row.try_get("pr_auto_complete_disabled")?,
        assignee_agent_id: parse_opt(row.try_get("assignee_agent_id")?)?,
        project_id: parse_opt(row.try_get("project_id")?)?,
        position: row.try_get("position")?,
        created_by: parse_opt(row.try_get("created_by")?)?,
        created_at: parse_ts(row.try_get("created_at")?)?,
        updated_at: parse_ts(row.try_get("updated_at")?)?,
        kind: parse(row.try_get("kind")?)?,
        source_task_id: parse_opt(row.try_get("source_task_id")?)?,
    })
}

fn activity_from(row: &SqliteRow) -> Result<TaskActivity> {
    Ok(TaskActivity {
        id: parse(row.try_get("id")?)?,
        task_id: parse(row.try_get("task_id")?)?,
        seq: row.try_get("seq")?,
        kind: parse(row.try_get("kind")?)?,
        actor_member_id: parse_opt(row.try_get("actor_member_id")?)?,
        body: json(row.try_get("body_json")?)?,
        created_at: parse_ts(row.try_get("created_at")?)?,
    })
}

fn run_from(row: &SqliteRow) -> Result<Run> {
    let budget: Budget = json(row.try_get("budget_json")?)?;
    Ok(Run {
        id: parse(row.try_get("id")?)?,
        task_id: parse(row.try_get("task_id")?)?,
        workflow_id: parse(row.try_get("workflow_id")?)?,
        workflow_version: u32_at(row, "workflow_version")?,
        status: parse(row.try_get("status")?)?,
        end_reason: row.try_get("end_reason")?,
        budget,
        repo_ids: json(row.try_get("repo_ids_json")?)?,
    })
}

fn step_from(row: &SqliteRow) -> Result<Step> {
    Ok(Step {
        id: parse(row.try_get("id")?)?,
        run_id: parse(row.try_get("run_id")?)?,
        step_key: StepKey::new(row.try_get::<String, _>("step_key")?)?,
        attempt: u32_at(row, "attempt")?,
        status: parse(row.try_get("status")?)?,
        runtime_id: parse_opt(row.try_get("runtime_id")?)?,
        session_id: parse_opt(row.try_get("session_id")?)?,
        started_at: parse_ts_opt(row.try_get("started_at")?)?,
        ended_at: parse_ts_opt(row.try_get("ended_at")?)?,
    })
}

pub(crate) fn event_from(row: &SqliteRow) -> Result<EventRecord> {
    Ok(EventRecord {
        seq: row.try_get("seq")?,
        run_id: parse(row.try_get("run_id")?)?,
        step_key: row
            .try_get::<Option<String>, _>("step_key")?
            .map(StepKey::new)
            .transpose()?,
        kind: row.try_get("kind")?,
        payload: json(row.try_get("payload_json")?)?,
        blob: row
            .try_get::<Option<String>, _>("blob")?
            .map(BlobHash::new)
            .transpose()?,
        at: parse_ts(row.try_get("at")?)?,
    })
}

fn outbox_from(row: &SqliteRow) -> Result<OutboxItem> {
    Ok(OutboxItem {
        id: parse(row.try_get("id")?)?,
        run_id: parse_opt(row.try_get("run_id")?)?,
        // `IdemKey` has no `FromStr`; its serde form is the plain string.
        idem_key: serde_json::from_value(Value::String(row.try_get("idem_key")?))?,
        target: row.try_get("target")?,
        payload: json(row.try_get("payload_json")?)?,
        status: parse(row.try_get("status")?)?,
        result: row
            .try_get::<Option<String>, _>("result_json")?
            .map(json)
            .transpose()?,
    })
}
