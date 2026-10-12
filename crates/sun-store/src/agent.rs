//! `agent`: each workspace's agents, and their hidden system workflows
//! (`workflow.system_agent_id`, on the agent's workspace).

use async_trait::async_trait;
use sqlx::Row;
use sqlx::sqlite::SqliteRow;
use sun_core::model::{Agent, Workflow};
use sun_core::{AgentId, WorkflowId, WorkspaceId};

use crate::repo::{parse, parse_ts, parse_ts_opt, ts, workflow_from};
use crate::{Result, StoreError, Tx};

/// **Repository** for `agent` rows and system workflows. Fields are checked
/// with [`Agent`]'s `clean_*` helpers by the caller; here names must be
/// unique among the workspace's active agents ignoring case.
#[async_trait]
pub trait AgentRepo {
    /// Inserts an agent; `InvalidInput` when an active agent has the name.
    async fn insert_agent(&mut self, agent: &Agent) -> Result<()>;
    /// One agent (archived too), or `NotFound`.
    async fn get_agent(&mut self, id: AgentId) -> Result<Agent>;
    /// The active agents of `workspace` (`None`: of every workspace), by name.
    async fn agents(&mut self, workspace: Option<WorkspaceId>) -> Result<Vec<Agent>>;
    /// Saves an agent's fields (`archived_at` too; never its workspace);
    /// `NotFound` or `InvalidInput` (name taken).
    async fn update_agent(&mut self, agent: &Agent) -> Result<()>;
    /// The number of open tasks (not Done or Cancelled) assigned to each
    /// agent, per workspace.
    async fn agent_task_counts(&mut self) -> Result<Vec<(AgentId, WorkspaceId, u32)>>;
    /// Inserts `workflow` as `agent`'s system workflow on its workspace.
    async fn insert_system_workflow(&mut self, workflow: &Workflow, agent: AgentId) -> Result<()>;
    /// `agent`'s system workflow on `workspace`, if it was made.
    async fn system_workflow(
        &mut self,
        agent: AgentId,
        workspace: WorkspaceId,
    ) -> Result<Option<WorkflowId>>;
    /// Every system workflow of `agent`.
    async fn system_workflows_of(&mut self, agent: AgentId) -> Result<Vec<WorkflowId>>;
    /// Every agent's system workflows, oldest first.
    async fn system_workflows(&mut self) -> Result<Vec<Workflow>>;
    /// The agent whose system workflow `workflow` is; `None` for a person's
    /// workflow (or a missing one).
    async fn system_agent_of(&mut self, workflow: WorkflowId) -> Result<Option<AgentId>>;
}

const COLUMNS: &str = "id, workspace_id, name, description, instructions, runtime_chain, \
                       skill_ids, created_at, archived_at";

#[async_trait]
impl AgentRepo for Tx {
    async fn insert_agent(&mut self, agent: &Agent) -> Result<()> {
        name_free(self, agent).await?;
        sqlx::query(&format!(
            "INSERT INTO agent ({COLUMNS}) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)"
        ))
        .bind(agent.id.to_string())
        .bind(agent.workspace_id.to_string())
        .bind(&agent.name)
        .bind(&agent.description)
        .bind(&agent.instructions)
        .bind(serde_json::to_string(&agent.runtime_chain)?)
        .bind(serde_json::to_string(&agent.skill_ids)?)
        .bind(ts(&agent.created_at))
        .bind(agent.archived_at.as_ref().map(ts))
        .execute(self.conn())
        .await?;
        Ok(())
    }

    async fn get_agent(&mut self, id: AgentId) -> Result<Agent> {
        let row = sqlx::query(&format!("SELECT {COLUMNS} FROM agent WHERE id = $1"))
            .bind(id.to_string())
            .fetch_optional(self.conn())
            .await?
            .ok_or_else(|| StoreError::not_found("agent", id))?;
        agent_from(&row)
    }

    async fn agents(&mut self, workspace: Option<WorkspaceId>) -> Result<Vec<Agent>> {
        sqlx::query(&format!(
            "SELECT {COLUMNS} FROM agent
             WHERE archived_at IS NULL AND ($1 IS NULL OR workspace_id = $1)
             ORDER BY lower(name), workspace_id"
        ))
        .bind(workspace.map(|w| w.to_string()))
        .fetch_all(self.conn())
        .await?
        .iter()
        .map(agent_from)
        .collect()
    }

    async fn update_agent(&mut self, agent: &Agent) -> Result<()> {
        if agent.archived_at.is_none() {
            name_free(self, agent).await?;
        }
        let done = sqlx::query(
            "UPDATE agent SET name = $1, description = $2, instructions = $3,
               runtime_chain = $4, skill_ids = $5, archived_at = $6
             WHERE id = $7",
        )
        .bind(&agent.name)
        .bind(&agent.description)
        .bind(&agent.instructions)
        .bind(serde_json::to_string(&agent.runtime_chain)?)
        .bind(serde_json::to_string(&agent.skill_ids)?)
        .bind(agent.archived_at.as_ref().map(ts))
        .bind(agent.id.to_string())
        .execute(self.conn())
        .await?;
        if done.rows_affected() == 0 {
            return Err(StoreError::not_found("agent", agent.id));
        }
        Ok(())
    }

    async fn agent_task_counts(&mut self) -> Result<Vec<(AgentId, WorkspaceId, u32)>> {
        let rows: Vec<(String, String, i64)> = sqlx::query_as(
            "SELECT assignee_agent_id, workspace_id, count(*) FROM task
             WHERE assignee_agent_id IS NOT NULL AND status NOT IN ('done', 'cancelled')
             GROUP BY assignee_agent_id, workspace_id",
        )
        .fetch_all(self.conn())
        .await?;
        rows.into_iter()
            .map(|(id, workspace, n)| {
                let n = u32::try_from(n).unwrap_or(u32::MAX);
                Ok((parse(id)?, parse(workspace)?, n))
            })
            .collect()
    }

    async fn insert_system_workflow(&mut self, workflow: &Workflow, agent: AgentId) -> Result<()> {
        sqlx::query(
            "INSERT INTO workflow (id, name, workspace_id, system_agent_id) VALUES ($1, $2, $3, $4)",
        )
        .bind(workflow.id.to_string())
        .bind(&workflow.name)
        .bind(workflow.workspace_id.to_string())
        .bind(agent.to_string())
        .execute(self.conn())
        .await?;
        Ok(())
    }

    async fn system_workflow(
        &mut self,
        agent: AgentId,
        workspace: WorkspaceId,
    ) -> Result<Option<WorkflowId>> {
        let id: Option<String> = sqlx::query_scalar(
            "SELECT id FROM workflow WHERE system_agent_id = $1 AND workspace_id = $2",
        )
        .bind(agent.to_string())
        .bind(workspace.to_string())
        .fetch_optional(self.conn())
        .await?;
        id.map(parse).transpose()
    }

    async fn system_workflows_of(&mut self, agent: AgentId) -> Result<Vec<WorkflowId>> {
        let ids: Vec<String> =
            sqlx::query_scalar("SELECT id FROM workflow WHERE system_agent_id = $1 ORDER BY id")
                .bind(agent.to_string())
                .fetch_all(self.conn())
                .await?;
        ids.into_iter().map(parse).collect()
    }

    async fn system_workflows(&mut self) -> Result<Vec<Workflow>> {
        sqlx::query(
            "SELECT id, name, workspace_id FROM workflow
             WHERE system_agent_id IS NOT NULL ORDER BY id",
        )
        .fetch_all(self.conn())
        .await?
        .iter()
        .map(workflow_from)
        .collect()
    }

    async fn system_agent_of(&mut self, workflow: WorkflowId) -> Result<Option<AgentId>> {
        let agent: Option<Option<String>> =
            sqlx::query_scalar("SELECT system_agent_id FROM workflow WHERE id = $1")
                .bind(workflow.to_string())
                .fetch_optional(self.conn())
                .await?;
        agent.flatten().map(parse).transpose()
    }
}

/// `InvalidInput` when another active agent of the workspace has the name
/// (any case).
async fn name_free(tx: &mut Tx, agent: &Agent) -> Result<()> {
    let taken: Option<String> = sqlx::query_scalar(
        "SELECT id FROM agent
         WHERE workspace_id = $1 AND lower(name) = lower($2) AND id <> $3
           AND archived_at IS NULL",
    )
    .bind(agent.workspace_id.to_string())
    .bind(&agent.name)
    .bind(agent.id.to_string())
    .fetch_optional(tx.conn())
    .await?;
    match taken {
        Some(_) => Err(sun_core::Error::InvalidInput(format!(
            "There is already an agent named `{}`.",
            agent.name
        ))
        .into()),
        None => Ok(()),
    }
}

fn agent_from(row: &SqliteRow) -> Result<Agent> {
    Ok(Agent {
        id: parse(row.try_get("id")?)?,
        workspace_id: parse(row.try_get("workspace_id")?)?,
        name: row.try_get("name")?,
        description: row.try_get("description")?,
        instructions: row.try_get("instructions")?,
        runtime_chain: serde_json::from_str(row.try_get("runtime_chain")?)?,
        skill_ids: serde_json::from_str(row.try_get("skill_ids")?)?,
        created_at: parse_ts(row.try_get("created_at")?)?,
        archived_at: parse_ts_opt(row.try_get("archived_at")?)?,
    })
}

/// Data step of `20261102090100_agent_copies` ([`crate::DATA_STEPS`]): each
/// agent used on a workspace other than its own, by a task assigned to it or
/// a system workflow, gets a copy there (a new id, the same fields), and that
/// workspace's tasks and system workflow move to the copy.
pub(crate) async fn copy_agents_into_their_workspaces(
    conn: &mut sqlx::SqliteConnection,
) -> Result<()> {
    let used: Vec<(String, String)> = sqlx::query_as(
        "SELECT DISTINCT a.id, u.workspace_id FROM agent a JOIN (
             SELECT system_agent_id AS agent_id, workspace_id FROM workflow
             WHERE system_agent_id IS NOT NULL
             UNION
             SELECT assignee_agent_id, workspace_id FROM task WHERE assignee_agent_id IS NOT NULL
         ) u ON u.agent_id = a.id
         WHERE u.workspace_id <> a.workspace_id
         ORDER BY a.id, u.workspace_id",
    )
    .fetch_all(&mut *conn)
    .await?;
    for (agent, workspace) in used {
        let copy = AgentId::generate().to_string();
        sqlx::query(&format!(
            "INSERT INTO agent ({COLUMNS})
             SELECT $1, $2, name, description, instructions, runtime_chain, skill_ids,
                    created_at, archived_at
             FROM agent WHERE id = $3"
        ))
        .bind(&copy)
        .bind(&workspace)
        .bind(&agent)
        .execute(&mut *conn)
        .await?;
        for table in [
            ("workflow", "system_agent_id"),
            ("task", "assignee_agent_id"),
        ] {
            sqlx::query(&format!(
                "UPDATE {0} SET {1} = $1 WHERE {1} = $2 AND workspace_id = $3",
                table.0, table.1
            ))
            .bind(&copy)
            .bind(&agent)
            .bind(&workspace)
            .execute(&mut *conn)
            .await?;
        }
    }
    Ok(())
}
