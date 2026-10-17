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

use serde_json::json;
use sun_core::model::{ActivityKind, Run, Task, TaskActivity, TaskKey, TaskKind};
use sun_core::redact::redact;
use sun_core::{
    AgentId, Error, MemberId, Result, RunId, RunStatus, TaskId, TaskStatus, WorkspaceId,
};
use sun_store::{AgentRepo, EventLog, RunRepo, SecretRepo, TaskRepo, Tx};

use crate::executor::tasks;
use crate::{Engine, OnWorkspace, agents};

/// The `run_status` reason of a run a comment queued.
pub(crate) const COMMENT: &str = "comment";
/// The latest comments a fresh session reads.
const THREAD_COMMENTS: usize = 20;
/// About 4k tokens, inside any context pack's budget.
const THREAD_CHARS: usize = 16_000;
/// Agents one comment may mention.
const MAX_MENTIONS: usize = 5;

/// `summary`, the done claim of `run_id`'s agent, as the agent's comment on
/// the run's task (a mention chat's: on the task it came from). Nothing for
/// an empty claim or another workflow's run.
pub(crate) async fn reply(tx: &mut Tx, run_id: RunId, summary: &str) -> Result<()> {
    let text = summary.trim();
    let run = tx.get_run(run_id).await?;
    let Some(agent) = tx.system_agent_of(run.workflow_id).await? else {
        return Ok(());
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
    let body = json!({ "text": text, "by_agent": agent, "run_id": run_id });
    let on = thread_of(tx, run.task_id).await?;
    tasks::log(tx, on, ActivityKind::Comment, None, body).await?;
    Ok(())
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

/// The run of the same workflow just before `run` on its task, unless the
/// watchdog stopped it: resuming a stalled, looping or over-budget session
/// would carry that context on, so those start fresh with the thread.
pub(crate) async fn previous_run(tx: &mut Tx, run: &Run) -> Result<Option<Run>> {
    let runs = tx.runs_of_task(run.task_id).await?;
    let before = runs.iter().take_while(|r| r.id != run.id);
    Ok(before
        .last()
        .filter(|r| r.workflow_id == run.workflow_id)
        .filter(|r| {
            !matches!(
                r.status,
                RunStatus::Stalled | RunStatus::Looping | RunStatus::BudgetExceeded
            )
        })
        .cloned())
}

/// The task's latest comments as `- who: text` lines, oldest first, `me`'s
/// marked "(you)"; empty without comments.
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
            _ => person(entry.body["by"].as_str().unwrap_or("someone")).to_owned(),
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
            let run = tx.runs_of_task(task.id).await?.pop();
            let last = tx
                .activity_of(task.id, 0)
                .await?
                .into_iter()
                .rfind(|a| a.kind == ActivityKind::Comment);
            let queued_behind = match (&run, task.assignee_agent_id) {
                (Some(run), Some(agent)) if run.status == RunStatus::Queued => {
                    busy(&mut tx, workspace, agent).await?
                }
                _ => Vec::new(),
            };
            out.push(Chat {
                task,
                run,
                last,
                queued_behind,
            });
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
