//! MCP connectors: the grants an agent session gets — the
//! workflow's `mcp:`, its step's `mcp:` and the task's own (stored as the
//! `kernel_setting` [`task_mcp_key`]) — and the secrets a registered MCP
//! server connects with.

use sun_core::model::SecretScope;
use sun_core::{Error, McpGrant, Result, RunId, StepKey, TaskId, WorkspaceId, render_mcp_config};
use sun_store::{RunRepo, SecretRepo, SettingRepo, Store, TaskRepo, WorkflowRepo};
use sun_workflow::StepKind;

use crate::Engine;

/// Most MCP servers one task may enable.
pub const MAX_TASK_MCP_GRANTS: usize = 32;
/// Most tools one grant may list.
const MAX_GRANT_TOOLS: usize = 256;

/// `kernel_setting` key of a task's MCP grants.
fn task_mcp_key(task_id: TaskId) -> String {
    format!("task_mcp.{task_id}")
}

/// Values of the secrets `names` (an MCP server's `secret_names()`), in
/// order, from Main's. A missing one is `InvalidInput` naming it.
// ponytail: MCP servers are deployment-wide, so their secrets are Main's;
// read the task's workspace's once servers are per workspace.
pub async fn mcp_secret_values(store: &Store, names: &[String]) -> Result<Vec<(String, String)>> {
    let scope = SecretScope::Workspace(WorkspaceId::MAIN);
    let mut read = store.read().await?;
    let mut values = Vec::with_capacity(names.len());
    for name in names {
        match read.secret_value(&scope, name).await.map_err(Error::from) {
            Ok(value) => values.push((name.clone(), value)),
            Err(Error::NotFound(_)) => {
                return Err(Error::InvalidInput(format!(
                    "secret {name} is not set in Main; add it under Settings → Secrets (or `sun secret set {name}`) first"
                )));
            }
            Err(e) => return Err(e),
        }
    }
    Ok(values)
}

/// A task's grants; none when never set.
async fn grants_of(read: &mut impl SettingRepo, task_id: TaskId) -> Result<Vec<McpGrant>> {
    match read.setting(&task_mcp_key(task_id)).await? {
        Some(value) => {
            serde_json::from_value(value).map_err(|e| Error::InvalidInput(e.to_string()))
        }
        None => Ok(Vec::new()),
    }
}

impl Engine {
    /// Values of the secrets an MCP server needs
    /// ([`mcp_secret_values`]).
    pub async fn mcp_secrets(&self, names: &[String]) -> Result<Vec<(String, String)>> {
        mcp_secret_values(&self.store, names).await
    }

    /// The MCP servers (and tools) the task's agent sessions get.
    pub async fn task_mcp(&self, task_id: TaskId) -> Result<Vec<McpGrant>> {
        let mut read = self.store.read().await?;
        read.get_task(task_id).await?;
        grants_of(&mut read, task_id).await
    }

    /// Replaces the task's grants; its next session gets them. The caller
    /// checks each server is registered (the registry is `sun-catalog`).
    pub async fn set_task_mcp(&self, task_id: TaskId, grants: &[McpGrant]) -> Result<()> {
        if grants.len() > MAX_TASK_MCP_GRANTS {
            return Err(Error::InvalidInput(format!(
                "at most {MAX_TASK_MCP_GRANTS} mcp servers per task"
            )));
        }
        if grants
            .iter()
            .any(|g| g.tools.as_ref().is_some_and(|t| t.len() > MAX_GRANT_TOOLS))
        {
            return Err(Error::InvalidInput(format!(
                "at most {MAX_GRANT_TOOLS} tools per mcp server"
            )));
        }
        // Rendering refuses a server granted twice.
        render_mcp_config(grants, "sun", &[])?;
        let value = serde_json::to_value(grants).map_err(|e| Error::InvalidInput(e.to_string()))?;
        let mut tx = self.store.begin().await?;
        tx.get_task(task_id).await?;
        tx.put_setting(&task_mcp_key(task_id), &value).await?;
        tx.commit().await?;
        Ok(())
    }
}

/// The grants a session of `step_key` in `run_id` gets: the workflow's
/// `mcp:`, then the step's, then the task's; a later grant for the same
/// server replaces an earlier one.
pub(crate) async fn session_grants(
    store: &Store,
    run_id: RunId,
    step_key: &StepKey,
) -> Result<Vec<McpGrant>> {
    let mut read = store.read().await?;
    let run = read.get_run(run_id).await?;
    let version = read
        .get_workflow_version(run.workflow_id, run.workflow_version)
        .await?;
    let workflow = sun_workflow::parse(&version.source_yaml)
        .map_err(|_| Error::InvalidInput(format!("workflow {} is invalid", run.workflow_id)))?;
    let step = workflow.steps.iter().find(|s| s.id == *step_key);
    let step_grants = match step.map(|s| &s.kind) {
        Some(StepKind::Agent { mcp, .. }) => mcp.clone(),
        _ => Vec::new(),
    };
    let task_grants = grants_of(&mut read, run.task_id).await?;
    let mut merged: Vec<McpGrant> = Vec::new();
    for grant in workflow
        .mcp
        .into_iter()
        .chain(step_grants)
        .chain(task_grants)
    {
        merged.retain(|g| g.server != grant.server);
        merged.push(grant);
    }
    Ok(merged)
}

impl Engine {
    /// The grant for `server` of the live session `(run_id, step_key)`
    /// (the kernel's MCP relay, which remote hosts reach); `Forbidden` when
    /// the session has none.
    pub async fn session_mcp_grant(
        &self,
        run_id: RunId,
        step_key: &StepKey,
        server: &str,
    ) -> Result<McpGrant> {
        session_grants(&self.store, run_id, step_key)
            .await?
            .into_iter()
            .find(|g| g.server == server)
            .ok_or_else(|| {
                Error::Forbidden(format!("mcp server {server} is not granted to this step"))
            })
    }
}
