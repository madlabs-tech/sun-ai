//! Talking with an agent on its task's thread.
//!
//! - **The agent replies as a comment.** The done claim of an agent's system
//!   workflow ([`reply`]) is a `comment` activity `{text, by_agent, run_id}`
//!   with no member, written in the transaction that stores the claim. An
//!   agent's comment never wakes anyone: only [`crate::Engine::comment`]
//!   (a person's) does.
//! - **A comment continues the conversation.** A run a comment queued
//!   ([`woken_by_comment`]) resumes the CLI session of the task's previous
//!   run (`executor::dispatch`); when it can't, the fresh session reads the
//!   thread ([`thread`]) before the messages left for it. Only comments
//!   resume: any other run (a move to Todo, a reassign) starts fresh with
//!   the thread.
//! - **An @mention brings an agent in.** A person's comment on a task may
//!   name agents ([`mentioned`]); then only they wake, not the assignee
//!   unless named. The assignee wakes on the task itself; any other agent in
//!   its mention chat ([`mention_chat`]): a hidden `chat` task, one per
//!   (task, agent), whose replies go to the task's thread and whose sessions
//!   read it.
//! - **A direct chat** ([`Engine::create_chat`]) is a `chat` task with no
//!   source: a person's own conversation with an agent, its messages the
//!   chat's comments. Only its creator sees it ([`Engine::chat_hidden`]).
//!   Without an agent ([`Engine::create_direct_chat`]) it runs its own
//!   hidden one-step workflow on the tool the person picked, with no
//!   instructions, skills, memory or workspace context, and the tool replies
//!   (`by_tool`).

use serde_json::json;
use sun_core::model::{
    ActivityKind, Run, Task, TaskActivity, TaskKey, TaskKind, Workflow, WorkflowOwner,
};
use sun_core::redact::redact;
use sun_core::{
    AgentId, ChainEntry, ChainSpec, Error, MemberId, PermissionMode, Result, RunId, RunStatus,
    StateMachine, TaskId, TaskStatus, WorkflowId, WorkspaceId,
};
use sun_store::{
    AgentRepo, EventLog, RunRepo, SecretRepo, TaskRepo, Tx, WorkflowRepo, WorkspaceRepo,
};

use crate::executor::tasks;
use crate::{Actor, Engine, OnWorkspace, agents};

/// The `run_status` reason of a run a comment queued.
pub(crate) const COMMENT: &str = "comment";
/// The latest comments a fresh session reads.
const THREAD_COMMENTS: usize = 20;
/// About 4k tokens, inside any context pack's budget.
const THREAD_CHARS: usize = 16_000;
/// Agents one comment may mention.
const MAX_MENTIONS: usize = 5;

/// `summary`, the done claim of `run_id`'s agent (or a direct chat's tool),
/// as its comment on the run's task (a mention chat's: on the task it came
/// from). Nothing for an empty claim or a person's workflow's run.
pub(crate) async fn reply(tx: &mut Tx, run_id: RunId, summary: &str) -> Result<()> {
    let text = summary.trim();
    let run = tx.get_run(run_id).await?;
    let (by, who) = match tx.workflow_owner(run.workflow_id).await? {
        WorkflowOwner::Agent(agent) => ("by_agent", json!(agent)),
        WorkflowOwner::DirectChat => ("by_tool", json!(chat_tool(tx, &run).await?)),
        WorkflowOwner::Person => return Ok(()),
    };
    if text.is_empty() {
        return Ok(());
    }
    // Redact before the cut: a secret split by it would no longer match.
    let secrets = tx.known_secrets().await?;
    let secrets: Vec<&str> = secrets.iter().map(String::as_str).collect();
    let text: String = redact(text, &secrets)
        .chars()
        .take(TaskActivity::MAX_COMMENT)
        .collect();
    let mut body = json!({ "text": text, "run_id": run_id });
    body[by] = who;
    let on = thread_of(tx, run.task_id).await?;
    tasks::log(tx, on, ActivityKind::Comment, None, body).await?;
    Ok(())
}

/// The tool a direct chat's run talks to, on the version it was pinned to.
async fn chat_tool(tx: &mut Tx, run: &Run) -> Result<String> {
    let version = tx
        .get_workflow_version(run.workflow_id, run.workflow_version)
        .await?;
    Ok(settings_of(&version.source_yaml)?.runtime.tool)
}

/// A chat without an agent's settings: its one chain entry and its step's
/// mode (`None`: the host's default).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct DirectSettings {
    /// The tool, and its credential, host, model and effort.
    pub runtime: ChainSpec,
    /// The mode its sessions run in.
    pub permission_mode: Option<PermissionMode>,
}

/// The settings a direct chat's workflow source carries.
fn settings_of(source_yaml: &str) -> Result<DirectSettings> {
    let parsed = crate::query::parse(source_yaml)?;
    let permission_mode = parsed
        .steps
        .first()
        .and_then(|step| parsed.permission_mode(&step.id));
    let runtime = parsed
        .runtime
        .and_then(|r| r.chain.into_iter().next())
        .and_then(|entry| entry.spec().cloned())
        .ok_or_else(|| Error::InvalidInput("A chat's workflow has no tool.".into()))?;
    Ok(DirectSettings {
        runtime,
        permission_mode,
    })
}

/// The settings of the chat's latest version (what its next message runs
/// on); `None` for a chat with an agent.
async fn direct_settings(tx: &mut Tx, task: &Task) -> Result<Option<DirectSettings>> {
    let Some(workflow) = task.workflow_id.filter(|_| task.kind == TaskKind::Chat) else {
        return Ok(None);
    };
    if tx.workflow_owner(workflow).await? != WorkflowOwner::DirectChat {
        return Ok(None);
    }
    let version = tx
        .latest_workflow_version(workflow)
        .await?
        .ok_or_else(|| Error::NotFound(format!("a saved version of workflow {workflow}")))?;
    Ok(Some(settings_of(&version.source_yaml)?))
}

/// A chat's title: its agent's name, or `{tool} · {model}` without one.
fn direct_title(runtime: &ChainSpec) -> String {
    match &runtime.model {
        Some(model) => format!("{} · {model}", runtime.tool),
        None => runtime.tool.clone(),
    }
}

/// `chat` as its list shows it.
async fn chat_of(tx: &mut Tx, task: Task) -> Result<Chat> {
    let run = tx.runs_of_task(task.id).await?.pop();
    let last = tx
        .activity_of(task.id, 0)
        .await?
        .into_iter()
        .rfind(|a| a.kind == ActivityKind::Comment);
    let queued_behind = match (&run, task.assignee_agent_id) {
        (Some(run), Some(agent)) if run.status == RunStatus::Queued => {
            busy(tx, task.workspace_id, agent).await?
        }
        _ => Vec::new(),
    };
    let direct = direct_settings(tx, &task).await?;
    Ok(Chat {
        task,
        run,
        last,
        queued_behind,
        direct,
    })
}

/// `PATCH /chats/{id}`: the settings to change on a chat without an agent;
/// absent ones stay.
#[derive(Debug, Clone, Default, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChatChange {
    /// The same tool, credential and host, with another model or effort.
    pub runtime: Option<ChainSpec>,
    /// `null`: the host's default.
    #[serde(default, deserialize_with = "crate::workspaces::present")]
    pub permission_mode: Option<Option<PermissionMode>>,
}

/// The task whose thread `task_id` talks on: its source for a mention chat.
pub(crate) async fn thread_of(tx: &mut Tx, task_id: TaskId) -> Result<TaskId> {
    Ok(tx
        .get_task(task_id)
        .await?
        .source_task_id
        .unwrap_or(task_id))
}

/// The agents a comment on `task` mentions, once each in their order: at
/// most five active agents of its workspace, and only on a board task.
pub(crate) async fn mentioned(tx: &mut Tx, task: &Task, ids: &[AgentId]) -> Result<Vec<AgentId>> {
    let mut out: Vec<AgentId> = Vec::new();
    for id in ids {
        if !out.contains(id) {
            out.push(*id);
        }
    }
    if out.is_empty() {
        return Ok(out);
    }
    if task.kind == TaskKind::Chat {
        return Err(Error::InvalidInput(
            "Mention agents on a task, not in a chat.".into(),
        ));
    }
    if out.len() > MAX_MENTIONS {
        return Err(Error::InvalidInput(format!(
            "Mention at most {MAX_MENTIONS} agents in one comment."
        )));
    }
    for id in &out {
        let active = match tx.get_agent(*id).await {
            Ok(agent) => agent.workspace_id == task.workspace_id && agent.archived_at.is_none(),
            Err(e) => match Error::from(e) {
                Error::NotFound(_) => false,
                e => return Err(e),
            },
        };
        if !active {
            return Err(Error::InvalidInput(
                "Mention active agents of this task's workspace.".into(),
            ));
        }
    }
    Ok(out)
}

/// The chat `agent` talks in about `source` (a board task), made on its
/// first mention. It waits in Needs you, like a run that finished, so the
/// comment that follows queues its run.
pub(crate) async fn mention_chat(tx: &mut Tx, source: &Task, agent: AgentId) -> Result<TaskId> {
    if let Some(chat) = tx.mention_chat(source.id, agent).await? {
        return Ok(chat.id);
    }
    let workflow = agents::system_workflow(tx, agent, source.workspace_id).await?;
    let chat = Task {
        kind: TaskKind::Chat,
        source_task_id: Some(source.id),
        workflow_id: Some(workflow),
        assignee_agent_id: Some(agent),
        status: TaskStatus::NeedsYou,
        ..Task::new(source.workspace_id, source.title.clone())
    };
    tx.insert_task(&chat).await?;
    Ok(chat.id)
}

/// True when a comment queued `run_id` (its first `run_status` reason).
pub(crate) async fn woken_by_comment(tx: &mut Tx, run_id: RunId) -> Result<bool> {
    let first = tx.events_after(Some(run_id), 0, 1).await?;
    Ok(first
        .first()
        .is_some_and(|e| e.payload["reason"] == COMMENT))
}

/// The run of the same workflow version just before `run` on its task,
/// unless the watchdog stopped it: resuming a stalled, looping or
/// over-budget session would carry that context on, so those start fresh
/// with the thread. A new version (an edited agent, a chat switched to
/// another model) starts fresh too: a resumed session keeps the
/// instructions and settings it started with.
pub(crate) async fn previous_run(tx: &mut Tx, run: &Run) -> Result<Option<Run>> {
    let runs = tx.runs_of_task(run.task_id).await?;
    let before = runs.iter().take_while(|r| r.id != run.id);
    Ok(before
        .last()
        .filter(|r| (r.workflow_id, r.workflow_version) == (run.workflow_id, run.workflow_version))
        .filter(|r| {
            !matches!(
                r.status,
                RunStatus::Stalled | RunStatus::Looping | RunStatus::BudgetExceeded
            )
        })
        .cloned())
}

/// The task's latest comments as `- who: text` lines, oldest first, `me`'s
/// (and a direct chat's tool's, the one reading) marked "(you)"; empty
/// without comments.
pub(crate) async fn thread(tx: &mut Tx, task_id: TaskId, me: Option<AgentId>) -> Result<String> {
    // ponytail: reads the whole log; page from the end when threads get long.
    let mut lines = Vec::new();
    for entry in tx.activity_of(task_id, 0).await? {
        if entry.kind != ActivityKind::Comment {
            continue;
        }
        let text = entry.body["text"].as_str().unwrap_or_default();
        let who = match entry.body["by_agent"].as_str().map(str::parse::<AgentId>) {
            Some(Ok(id)) if Some(id) == me => format!("{} (you)", tx.get_agent(id).await?.name),
            Some(Ok(id)) => tx.get_agent(id).await?.name,
            _ => match entry.body["by_tool"].as_str() {
                Some(tool) => format!("{tool} (you)"),
                None => person(entry.body["by"].as_str().unwrap_or("someone")).to_owned(),
            },
        };
        lines.push(format!("- {who}: {text}"));
    }
    let mut keep = lines.len().saturating_sub(THREAD_COMMENTS);
    while keep < lines.len()
        && lines[keep..]
            .iter()
            .map(|l| l.chars().count())
            .sum::<usize>()
            > THREAD_CHARS
    {
        keep += 1;
    }
    if keep == lines.len() {
        return Ok(String::new());
    }
    Ok(format!(
        "The conversation on this task so far, oldest first:\n{}",
        lines[keep..].join("\n")
    ))
}

/// A person's name from their audit actor (`user:ana` → `ana`).
pub(crate) fn person(actor: &str) -> &str {
    actor.split_once(':').map_or(actor, |(_, name)| name)
}

/// A direct chat as its list shows it ([`Engine::chats`]).
#[derive(Debug, Clone)]
pub struct Chat {
    /// The chat.
    pub task: Task,
    /// Its latest run.
    pub run: Option<Run>,
    /// Its latest message.
    pub last: Option<TaskActivity>,
    /// While its run waits: the agent's tasks with a run going, which hold
    /// the slots it waits for.
    pub queued_behind: Vec<TaskKey>,
    /// Without an agent: the tool and settings it talks to.
    pub direct: Option<DirectSettings>,
}

impl Engine {
    /// A new chat of `creator` (`None`: the token holder) with `agent`, on
    /// the agent's workspace. It waits in Needs you for its first message.
    /// `InvalidInput` for an archived agent.
    pub async fn create_chat(&self, agent: AgentId, creator: Option<MemberId>) -> Result<Task> {
        let mut tx = self.store.begin().await?;
        let agent = tx.get_agent(agent).await?;
        let workflow = agents::system_workflow(&mut tx, agent.id, agent.workspace_id).await?;
        let chat = Task {
            kind: TaskKind::Chat,
            workflow_id: Some(workflow),
            assignee_agent_id: Some(agent.id),
            status: TaskStatus::NeedsYou,
            created_by: creator,
            ..Task::new(agent.workspace_id, format!("Chat with {}", agent.name))
        };
        tx.insert_task(&chat).await?;
        let chat = tx.get_task(chat.id).await?;
        tx.commit().await?;
        Ok(chat)
    }

    /// A new chat of `creator` (`None`: the token holder) on `workspace`
    /// with no agent: straight with `runtime`'s tool, in `permission_mode`
    /// (`None`: the host's default). It waits in Needs you for its first
    /// message. `InvalidInput` for an entry or a mode `chain_check` refuses.
    pub async fn create_direct_chat(
        &self,
        workspace: WorkspaceId,
        runtime: ChainSpec,
        permission_mode: Option<PermissionMode>,
        creator: Option<MemberId>,
    ) -> Result<Chat> {
        let title = direct_title(&runtime);
        let chain = [ChainEntry::Spec(runtime)];
        let (source, parsed) = agents::one_step_source(&chain, "", &[], permission_mode)?;
        let mut tx = self.store.begin().await?;
        tx.get_workspace(workspace).await?;
        crate::chain_check::check(&mut tx, workspace, &chain, "runtime").await?;
        crate::chain_check::check_mode(
            &mut tx,
            workspace,
            &chain,
            permission_mode,
            agents::MODE_FIELD,
        )
        .await?;
        let workflow = Workflow {
            id: WorkflowId::generate(),
            workspace_id: workspace,
            name: title.clone(),
        };
        tx.insert_direct_chat_workflow(&workflow).await?;
        tx.save_workflow_version(workflow.id, &source, &parsed.hash)
            .await?;
        let chat = Task {
            kind: TaskKind::Chat,
            workflow_id: Some(workflow.id),
            status: TaskStatus::NeedsYou,
            created_by: creator,
            ..Task::new(workspace, title)
        };
        tx.insert_task(&chat).await?;
        let task = tx.get_task(chat.id).await?;
        let chat = chat_of(&mut tx, task).await?;
        tx.commit().await?;
        Ok(chat)
    }

    /// Switches a chat without an agent to `change`'s model, effort or mode:
    /// a new version of its workflow, so its next message starts a fresh
    /// session that reads the thread. Logged as a `settings` activity; the
    /// title follows the model. `InvalidInput` for a chat with an agent, a
    /// change of tool, credential or host (one per thread: start a new
    /// chat), or while a run is queued or going (it is pinned to the old
    /// version); a stopped (paused) run is cancelled. Nothing changes when
    /// nothing differs.
    pub async fn update_direct_chat(
        &self,
        id: TaskId,
        change: ChatChange,
        actor: Option<&Actor>,
    ) -> Result<Chat> {
        let mut tx = self.store.begin().await?;
        let mut task = tx.get_task(id).await?;
        let Some(before) = direct_settings(&mut tx, &task).await? else {
            return Err(Error::InvalidInput(
                "Only a chat without an agent has settings to switch.".into(),
            ));
        };
        let after = DirectSettings {
            runtime: change.runtime.unwrap_or_else(|| before.runtime.clone()),
            permission_mode: change.permission_mode.unwrap_or(before.permission_mode),
        };
        let (old, new) = (&before.runtime, &after.runtime);
        if (&old.tool, &old.credential, &old.host) != (&new.tool, &new.credential, &new.host) {
            return Err(Error::InvalidInput(
                "Start a new chat to use another tool, credential or host.".into(),
            ));
        }
        if after == before {
            let chat = chat_of(&mut tx, task).await?;
            tx.commit().await?;
            return Ok(chat);
        }
        let chain = [ChainEntry::Spec(after.runtime.clone())];
        crate::chain_check::check(&mut tx, task.workspace_id, &chain, "runtime").await?;
        let mode = after.permission_mode;
        crate::chain_check::check_mode(
            &mut tx,
            task.workspace_id,
            &chain,
            mode,
            agents::MODE_FIELD,
        )
        .await?;
        // A stopped reply pauses its run, pinned to the old version: end it,
        // or the next message would resume the old session. The executor
        // owns the cancel and the store has one writer, so this tx yields.
        let last = tx.runs_of_task(id).await?.pop();
        if let Some(run) = last.filter(|r| r.status == RunStatus::Paused) {
            drop(tx);
            self.cancel(run.id).await?;
            tx = self.store.begin().await?;
            task = tx.get_task(id).await?;
        }
        let going = tx.runs_of_task(id).await?.pop();
        if going.is_some_and(|r| !r.status.is_terminal()) {
            return Err(Error::InvalidInput(
                "Wait for the reply or stop it, then switch.".into(),
            ));
        }
        let (source, parsed) = agents::one_step_source(&chain, "", &[], after.permission_mode)?;
        let workflow = task
            .workflow_id
            .ok_or_else(|| Error::NotFound(format!("chat {id}")))?;
        tx.save_workflow_version(workflow, &source, &parsed.hash)
            .await?;
        let title = direct_title(&after.runtime);
        if task.title != title {
            task.title = title;
            tx.update_task(&task).await?;
            tx.rename_workflow(workflow, &task.title).await?;
        }
        let body = json!({ "from": before, "to": after });
        tasks::log(&mut tx, id, ActivityKind::Settings, actor, body).await?;
        let task = tx.get_task(id).await?;
        let chat = chat_of(&mut tx, task).await?;
        tx.commit().await?;
        Ok(chat)
    }

    /// The direct chats `creator` started on `workspace`, last active first.
    // ponytail: a few reads per chat; batch them when people keep many chats.
    pub async fn chats(
        &self,
        workspace: WorkspaceId,
        creator: Option<MemberId>,
    ) -> Result<Vec<Chat>> {
        let mut tx = self.store.read().await?;
        let mut out = Vec::new();
        for task in tx.chats_of(workspace, creator).await? {
            out.push(chat_of(&mut tx, task).await?);
        }
        Ok(out)
    }

    /// True when `thing` is a direct chat, or a run of one, that `member`
    /// didn't start: it is hidden from them like a missing one.
    pub async fn chat_hidden(
        &self,
        thing: OnWorkspace<'_>,
        member: Option<MemberId>,
    ) -> Result<bool> {
        let mut tx = self.store.read().await?;
        let found = match thing {
            OnWorkspace::Task(id) => tx.get_task(id).await,
            OnWorkspace::Run(id) => match tx.get_run(id).await {
                Ok(run) => tx.get_task(run.task_id).await,
                Err(e) => Err(e),
            },
            _ => return Ok(false),
        };
        match found.map_err(Error::from) {
            Ok(task) => Ok(task.kind == TaskKind::Chat
                && task.source_task_id.is_none()
                && task.created_by != member),
            Err(Error::NotFound(_)) => Ok(false),
            Err(e) => Err(e),
        }
    }
}

/// The keys of `agent`'s board tasks on `workspace` whose latest run is running.
async fn busy(tx: &mut Tx, workspace: WorkspaceId, agent: AgentId) -> Result<Vec<TaskKey>> {
    let mut keys = Vec::new();
    for task in tx.tasks_of_workspace(workspace).await? {
        if task.assignee_agent_id != Some(agent) {
            continue;
        }
        let running = tx
            .runs_of_task(task.id)
            .await?
            .pop()
            .is_some_and(|r| r.status == RunStatus::Running);
        if running {
            keys.push(tasks::key_of(tx, &task).await?);
        }
    }
    Ok(keys)
}
