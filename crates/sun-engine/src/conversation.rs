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
//!   thread ([`thread`]) before the messages left for it.

use serde_json::json;
use sun_core::model::{ActivityKind, Run, TaskActivity};
use sun_core::{AgentId, Result, RunId, TaskId};
use sun_store::{AgentRepo, EventLog, RunRepo, TaskRepo, Tx};

use crate::executor::tasks;

/// The `run_status` reason of a run a comment queued.
pub(crate) const COMMENT: &str = "comment";
/// The latest comments a fresh session reads.
const THREAD_COMMENTS: usize = 20;
/// About 4k tokens, inside any context pack's budget.
const THREAD_CHARS: usize = 16_000;

/// `summary`, the done claim of `run_id`'s agent, as the agent's comment on
/// the run's task. Nothing for an empty claim or another workflow's run.
pub(crate) async fn reply(tx: &mut Tx, run_id: RunId, summary: &str) -> Result<()> {
    let text = summary.trim();
    let run = tx.get_run(run_id).await?;
    let Some(agent) = tx.system_agent_of(run.workflow_id).await? else {
        return Ok(());
    };
    if text.is_empty() {
        return Ok(());
    }
    let text: String = text.chars().take(TaskActivity::MAX_COMMENT).collect();
    let body = json!({ "text": text, "by_agent": agent, "run_id": run_id });
    tasks::log(tx, run.task_id, ActivityKind::Comment, None, body).await?;
    Ok(())
}

/// True when a comment queued `run_id` (its first `run_status` reason).
pub(crate) async fn woken_by_comment(tx: &mut Tx, run_id: RunId) -> Result<bool> {
    let first = tx.events_after(Some(run_id), 0, 1).await?;
    Ok(first
        .first()
        .is_some_and(|e| e.payload["reason"] == COMMENT))
}

/// The run of the same workflow just before `run` on its task.
pub(crate) async fn previous_run(tx: &mut Tx, run: &Run) -> Result<Option<Run>> {
    let runs = tx.runs_of_task(run.task_id).await?;
    let before = runs.iter().take_while(|r| r.id != run.id);
    Ok(before
        .last()
        .filter(|r| r.workflow_id == run.workflow_id)
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
    while keep < lines.len() && lines[keep..].iter().map(String::len).sum::<usize>() > THREAD_CHARS
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
