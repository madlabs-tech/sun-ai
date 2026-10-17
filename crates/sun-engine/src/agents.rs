//! Agents, part of the **Facade** for the API: each workspace's agents, which
//! its tasks can be assigned to (never another workspace's). Each change
//! commits with its audit row.
//!
//! An agent runs through the engine like any workflow: a task assigned to
//! it runs the agent's hidden **system workflow** on its workspace
//! (one `agent` step with its runtime chain, instructions and skills, no
//! approval), made on the first assignment there ([`system_workflow`]).
//! Editing the agent saves a new version of each of its system workflows;
//! runs keep the version they were pinned to. System workflows are left out
//! of every workflow list, and only the engine saves them.
//!
//! A session's prompt is the agent's instructions, then its task (key,
//! title, description) ([`with_task`]); the engine adds the messages left
//! for the step, such as the comment that woke it.

use std::collections::HashMap;

use serde_json::json;
use sun_core::entitlement::{Entitlement, Quota};
use sun_core::model::{Agent, AuditAction, Run, TaskKind, Workflow};
use sun_core::{AgentId, Error, Result, WorkflowId, WorkspaceId};
use sun_store::{AgentRepo, AuditRepo, RepoRepo, TaskRepo, Tx, WorkflowRepo, WorkspaceRepo};

use crate::Engine;
use crate::executor::tasks;
use crate::query::parse;

/// The step every system workflow has, and its YAML `name:` (a slug; the
/// workflow row is named after the agent).
pub const AGENT_STEP: &str = "agent";

/// `POST /agents` and `PATCH /agents/{id}`: the fields to set; absent ones
/// stay (a new agent needs a name and a runtime).
#[derive(Debug, Clone, Default, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentFields {
    /// 1–64 characters, unique among the workspace's active agents ignoring case.
    pub name: Option<String>,
    /// At most 1000 characters.
    pub description: Option<String>,
    /// Its prompt, at most 20 000 characters.
    pub instructions: Option<String>,
    /// Runtimes in failover order, names or `{tool, credential, host,
    /// model, effort}`; 1–32.
    pub runtime_chain: Option<Vec<sun_core::ChainEntry>>,
    /// Skill names (catalog ids); at most 32.
    pub skill_ids: Option<Vec<String>>,
    /// The mode its sessions run in; `null` inherits the tool's
    /// `runtime_policy` row.
    #[serde(default, deserialize_with = "crate::workspaces::present")]
    pub permission_mode: Option<Option<sun_core::PermissionMode>>,
}

impl AgentFields {
    /// `agent` with these fields set and checked; its system workflow source
    /// must parse.
    fn apply(self, mut agent: Agent) -> Result<Agent> {
        if let Some(name) = self.name {
            agent.name = Agent::clean_name(&name)?;
        }
        if let Some(text) = self.description {
            agent.description = Agent::clean_description(&text)?;
        }
        if let Some(text) = self.instructions {
            agent.instructions = Agent::clean_instructions(&text)?;
        }
        if let Some(chain) = self.runtime_chain {
            agent.runtime_chain = chain;
        }
        if let Some(skills) = self.skill_ids {
            agent.skill_ids = Agent::clean_skills(skills)?;
        }
        if let Some(mode) = self.permission_mode {
            agent.permission_mode = mode;
        }
        agent.name = Agent::clean_name(&agent.name)?;
        agent.runtime_chain = Agent::clean_chain(agent.runtime_chain)?;
        system_source(&agent)?;
        Ok(agent)
    }
}

/// The YAML (JSON, a YAML subset) of `agent`'s system workflow, with its
/// parse: one `agent` step, the agent's chain, skills and permission mode
/// (left out when it inherits, so older agents keep their source). `InvalidInput`
/// when the parser refuses a runtime or skill name.
pub(crate) fn system_source(agent: &Agent) -> Result<(String, sun_workflow::Workflow)> {
    // No instructions: an empty prompt, the session gets its task alone.
    // The newline keeps a one-word prompt like `fix.md` from reading as a
    // prompt file.
    let prompt = match agent.instructions.trim() {
        "" => String::new(),
        text => format!("{text}\n"),
    };
    let mut step = json!({
        "id": AGENT_STEP,
        "type": "agent",
        "prompt": prompt,
        "skills": agent.skill_ids,
    });
    if let Some(mode) = agent.permission_mode {
        step["permission_mode"] = json!(mode);
    }
    let source = json!({
        "name": AGENT_STEP,
        "runtime": { "chain": agent.runtime_chain },
        "steps": [step],
    });
    let source = serde_json::to_string_pretty(&source)
        .map_err(|e| Error::InvalidInput(format!("agent {}: {e}", agent.name)))?;
    let parsed = parse(&source)?;
    Ok((source, parsed))
}

/// `prompt` (a step's) for a session of `run`: on an agent's system
/// workflow, its instructions followed by the task's key, title and
/// description (a mention chat's: the task it was mentioned on, and no
/// repositories), then the workspace's repositories and how to work in them
/// (the session runs in an empty folder and clones what it needs with the
/// host machine's own git credentials); any other workflow's as it is.
// ponytail: add `sun repo checkout <url>` reusing `Worktrees::fetch`'s bare
// cache when clone time hurts.
pub(crate) async fn with_task(tx: &mut Tx, run: &Run, prompt: String) -> Result<String> {
    if tx.system_agent_of(run.workflow_id).await?.is_none() {
        return Ok(prompt);
    }
    let mut task = tx.get_task(run.task_id).await?;
    let mut out = match prompt.trim() {
        "" => String::new(),
        instructions => format!("{instructions}\n\n"),
    };
    if task.kind == TaskKind::Chat && task.source_task_id.is_none() {
        out.push_str(
            "You are chatting with a person. Answer their messages; your final \
             message is posted in the chat as your reply.",
        );
        return Ok(out);
    }
    // A mention chat: the task it came from, without its repos (a chat
    // answers; the task's own agent does its work).
    let mentioned = task.source_task_id.is_some();
    if let Some(source) = task.source_task_id {
        task = tx.get_task(source).await?;
    }
    let key = tasks::key_of(tx, &task).await?;
    match mentioned {
        true => out.push_str(&format!(
            "You were mentioned on task {key} {}. Your final message is posted \
             there as your reply.",
            task.title
        )),
        false => out.push_str(&format!("Your task: {key} {}", task.title)),
    }
    if !task.body.trim().is_empty() {
        out.push_str(&format!("\n\n{}", task.body.trim()));
    }
    if mentioned {
        return Ok(out);
    }
    let repos = tx.repos(task.workspace_id).await?;
    if !repos.is_empty() {
        out.push_str("\n\n## Repositories agents can use\n");
        for repo in &repos {
            match repo.description.trim() {
                "" => out.push_str(&format!("\n- {}", repo.url)),
                what => out.push_str(&format!("\n- {} — {what}", repo.url)),
            }
        }
        out.push_str(&format!(
            "\n\nClone what you need into this folder. Work on branch `{}`, push with \
             this machine's git credentials, and open a pull request whose title \
             starts with `{key}`.",
            key.branch(&task.title)
        ));
    }
    Ok(out)
}

/// `agent`'s system workflow on `workspace`, made with its first version on
/// the first assignment there. `NotFound` for another workspace's agent,
/// `InvalidInput` for an archived one.
pub(crate) async fn system_workflow(
    tx: &mut Tx,
    agent: AgentId,
    workspace: WorkspaceId,
) -> Result<WorkflowId> {
    let agent = tx.get_agent(agent).await?;
    if agent.workspace_id != workspace {
        return Err(Error::NotFound(format!("agent {}", agent.id)));
    }
    if agent.archived_at.is_some() {
        return Err(Error::InvalidInput(format!(
            "The agent {} is archived; pick another one.",
            agent.name
        )));
    }
    if let Some(id) = tx.system_workflow(agent.id, workspace).await? {
        return Ok(id);
    }
    tx.get_workspace(workspace).await?;
    let (source, parsed) = system_source(&agent)?;
    let workflow = Workflow {
        id: WorkflowId::generate(),
        workspace_id: workspace,
        name: agent.name.clone(),
    };
    tx.insert_system_workflow(&workflow, agent.id).await?;
    tx.save_workflow_version(workflow.id, &source, &parsed.hash)
        .await?;
    Ok(workflow.id)
}

impl Engine {
    /// The active agents of `workspace` (`None`: of every workspace), by name.
    pub async fn agents(&self, workspace: Option<WorkspaceId>) -> Result<Vec<Agent>> {
        Ok(self.store.read().await?.agents(workspace).await?)
    }

    /// One agent, archived too; `NotFound` when there is none.
    pub async fn agent(&self, id: AgentId) -> Result<Agent> {
        Ok(self.store.read().await?.get_agent(id).await?)
    }

    /// Open tasks (not Done or Cancelled) per agent, on the workspaces
    /// `sees` lets through (the caller's).
    pub async fn agent_task_counts(
        &self,
        sees: impl Fn(WorkspaceId) -> bool,
    ) -> Result<HashMap<AgentId, u32>> {
        let mut tx = self.store.read().await?;
        let mut counts = HashMap::new();
        for (agent, workspace, n) in tx.agent_task_counts().await? {
            if sees(workspace) {
                *counts.entry(agent).or_default() += n;
            }
        }
        Ok(counts)
    }

    /// Every agent's system workflows with their latest version numbers
    /// (task views name them; the lists leave them out).
    pub async fn system_workflows(&self) -> Result<Vec<(Workflow, u32)>> {
        let mut tx = self.store.read().await?;
        let mut out = Vec::new();
        for workflow in tx.system_workflows().await? {
            let version = tx
                .latest_workflow_version(workflow.id)
                .await?
                .map_or(0, |v| v.version);
            out.push((workflow, version));
        }
        Ok(out)
    }

    /// Creates an agent on `workspace` from `fields` (a name and a runtime
    /// at least); `NotFound` for a missing workspace, `PlanLimit` when
    /// `plan` has no room for another agent.
    pub async fn create_agent(
        &self,
        workspace: WorkspaceId,
        fields: AgentFields,
        actor: &str,
        plan: &Entitlement,
    ) -> Result<Agent> {
        let agent = fields.apply(Agent {
            id: AgentId::generate(),
            workspace_id: workspace,
            name: String::new(),
            description: String::new(),
            instructions: String::new(),
            runtime_chain: Vec::new(),
            skill_ids: Vec::new(),
            permission_mode: None,
            created_at: chrono::Utc::now(),
            archived_at: None,
        })?;
        let mut tx = self.store.begin().await?;
        tx.get_workspace(workspace).await?;
        crate::chain_check::check(&mut tx, workspace, &agent.runtime_chain, "runtime_chain")
            .await?;
        plan.check(Quota::Agents, tx.agents(Some(workspace)).await?.len())?;
        tx.insert_agent(&agent).await?;
        audit(&mut tx, &agent, actor, "created").await?;
        tx.commit().await?;
        Ok(agent)
    }

    /// Changes the fields `fields` names, and saves a new version of each of
    /// the agent's system workflows whose latest one isn't what the agent
    /// generates now, so any save also regenerates a version an older engine
    /// made. An archived agent doesn't change.
    pub async fn update_agent(
        &self,
        id: AgentId,
        fields: AgentFields,
        actor: &str,
    ) -> Result<Agent> {
        let mut tx = self.store.begin().await?;
        let before = tx.get_agent(id).await?;
        if before.archived_at.is_some() {
            return Err(Error::InvalidInput(format!(
                "The agent {} is archived.",
                before.name
            )));
        }
        let agent = fields.apply(before.clone())?;
        // A chain saved before stays as it is even if a host left since.
        if agent.runtime_chain != before.runtime_chain {
            let chain = &agent.runtime_chain;
            crate::chain_check::check(&mut tx, agent.workspace_id, chain, "runtime_chain").await?;
        }
        let (source, parsed) = system_source(&agent)?;
        for workflow in tx.system_workflows_of(id).await? {
            if tx.get_workflow(workflow).await?.name != agent.name {
                tx.rename_workflow(workflow, &agent.name).await?;
            }
            let latest = tx.latest_workflow_version(workflow).await?;
            if latest.is_none_or(|v| v.source_yaml != source) {
                tx.save_workflow_version(workflow, &source, &parsed.hash)
                    .await?;
            }
        }
        if agent != before {
            tx.update_agent(&agent).await?;
            let what = format!("updated (was {})", before.name);
            audit(&mut tx, &agent, actor, &what).await?;
        }
        tx.commit().await?;
        Ok(agent)
    }

    /// Archives an agent (`DELETE`): it leaves the list and takes no new
    /// tasks; its tasks and runs stay. Archiving twice changes nothing.
    pub async fn archive_agent(&self, id: AgentId, actor: &str) -> Result<()> {
        let mut tx = self.store.begin().await?;
        let mut agent = tx.get_agent(id).await?;
        if agent.archived_at.is_some() {
            return Ok(());
        }
        agent.archived_at = Some(chrono::Utc::now());
        tx.update_agent(&agent).await?;
        audit(&mut tx, &agent, actor, "archived").await?;
        Ok(tx.commit().await?)
    }
}

/// An `agent_change` audit row on the agent's name: `<what>`.
async fn audit(tx: &mut Tx, agent: &Agent, actor: &str, what: &str) -> Result<()> {
    tx.append_audit(actor, AuditAction::AgentChange, &agent.name, what)
        .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn agent(instructions: &str, chain: &[&str]) -> Agent {
        Agent {
            id: AgentId::generate(),
            workspace_id: WorkspaceId::MAIN,
            name: "Ada".into(),
            description: String::new(),
            instructions: instructions.into(),
            runtime_chain: chain.iter().map(|r| (*r).into()).collect(),
            skill_ids: vec!["tdd".into()],
            permission_mode: None,
            created_at: chrono::Utc::now(),
            archived_at: None,
        }
    }

    #[test]
    fn agent_system_workflow_is_one_agent_step_with_its_chain_and_skills() {
        let (_, parsed) = system_source(&agent("fix.md", &["claude-code", "codex"])).unwrap();
        assert_eq!(parsed.name, AGENT_STEP);
        assert_eq!(
            parsed.runtime.as_ref().map(|r| r.chain.clone()),
            Some(vec!["claude-code".into(), "codex".into()])
        );
        let [step] = parsed.steps.as_slice() else {
            panic!("{:?}", parsed.steps)
        };
        let sun_workflow::StepKind::Agent { prompt, skills, .. } = &step.kind else {
            panic!("{:?}", step.kind)
        };
        assert!(!sun_workflow::is_prompt_file(prompt), "{prompt}");
        assert_eq!(skills, &["tdd"]);
        let (_, parsed) = system_source(&agent("  ", &["codex"])).unwrap();
        let sun_workflow::StepKind::Agent { prompt, .. } = &parsed.steps[0].kind else {
            panic!()
        };
        assert_eq!(prompt, "");
        // The parser checks runtime names.
        assert!(matches!(
            system_source(&agent("", &["not a runtime"])),
            Err(Error::InvalidInput(_))
        ));
    }

    #[test]
    fn an_agents_permission_mode_is_its_system_steps() {
        let mut ada = agent("fix it", &["claude-code"]);
        let (inherits, parsed) = system_source(&ada).unwrap();
        let key = sun_core::StepKey::new(AGENT_STEP).unwrap();
        assert_eq!(parsed.permission_mode(&key), None);
        assert!(!inherits.contains("permission_mode"), "{inherits}");
        ada.permission_mode = Some(sun_core::PermissionMode::Plan);
        let (_, parsed) = system_source(&ada).unwrap();
        assert_eq!(parsed.permission_mode(&key), ada.permission_mode);
    }
}
