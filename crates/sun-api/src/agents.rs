//! Agents: `GET|POST /agents?workspace=`, `PATCH|DELETE /agents/{id}`
//! (DELETE archives). An agent belongs to one workspace: the guard keeps
//! everyone but its members off it, any member reads, and writes need
//! `manage_runtimes` there (the router). Only a deployment owner may leave
//! out `?workspace=` (every workspace's list; a new agent goes to Main).

use std::collections::BTreeMap;

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::{Extension, Json};
use serde::Serialize;
use sun_core::model::Agent;
use sun_core::{AgentId, Timestamp, WorkspaceId};
use sun_engine::AgentFields;

use crate::Api;
use crate::auth::Caller;
use crate::entitlement::within_plan;
use crate::error::ApiResult;
use crate::hosts::{online_adapters, readiness_of};
use crate::routes::{Body, id};
use crate::workspace_access::{InWorkspace, listed_workspace, named_workspace};
use sun_core::entitlement::Quota;

/// `Agent` in `types.ts`: the row, whether it can run now, and its open tasks.
#[derive(Debug, Serialize)]
pub(crate) struct AgentView {
    id: AgentId,
    workspace_id: WorkspaceId,
    name: String,
    description: String,
    instructions: String,
    runtime_chain: Vec<String>,
    skill_ids: Vec<String>,
    created_at: Timestamp,
    archived_at: Option<Timestamp>,
    /// Some runtime of the chain is logged in on an online host.
    ready: bool,
    /// Why not, per runtime of the chain; `null` when ready.
    ready_reason: Option<String>,
    /// Its tasks that aren't Done or Cancelled, on workspaces the caller sees.
    task_count: u32,
}

/// The view of `agent`, its readiness read from `online`.
async fn view(
    api: &Api,
    agent: Agent,
    online: &BTreeMap<String, (bool, bool)>,
    task_count: u32,
) -> ApiResult<AgentView> {
    let mut reasons = Vec::new();
    let mut ready = false;
    for name in &agent.runtime_chain {
        let reason = match readiness_of(api, agent.workspace_id, name.clone(), online).await {
            Ok(r) if r.installed && r.logged_in => {
                ready = true;
                break;
            }
            Ok(r) => r.detail.unwrap_or_else(|| "not ready".into()),
            Err(_) => "no host reports it".into(),
        };
        reasons.push(format!("{name}: {reason}"));
    }
    Ok(AgentView {
        id: agent.id,
        workspace_id: agent.workspace_id,
        name: agent.name,
        description: agent.description,
        instructions: agent.instructions,
        runtime_chain: agent.runtime_chain,
        skill_ids: agent.skill_ids,
        created_at: agent.created_at,
        archived_at: agent.archived_at,
        ready,
        ready_reason: (!ready).then(|| reasons.join("; ")),
        task_count,
    })
}

/// One agent's view for `caller`, read now.
async fn one(api: &Api, caller: &Caller, agent: Agent) -> ApiResult<AgentView> {
    let online = online_adapters(api).await?;
    let counts = api.engine.agent_task_counts(|w| caller.sees(w)).await?;
    let count = counts.get(&agent.id).copied().unwrap_or(0);
    view(api, agent, &online, count).await
}

/// The workspace's active agents, by name.
pub(crate) async fn list(
    State(api): State<Api>,
    Extension(caller): Extension<Caller>,
    Query(query): Query<InWorkspace>,
) -> ApiResult<Json<Vec<AgentView>>> {
    let workspace = listed_workspace(&caller, query.workspace.as_deref())?;
    let online = online_adapters(&api).await?;
    let counts = api.engine.agent_task_counts(|w| caller.sees(w)).await?;
    let mut out = Vec::new();
    for agent in api.engine.agents(workspace).await? {
        let count = counts.get(&agent.id).copied().unwrap_or(0);
        out.push(view(&api, agent, &online, count).await?);
    }
    Ok(Json(out))
}

/// `POST /agents?workspace= {name, runtime_chain, description?,
/// instructions?, skill_ids?}`.
pub(crate) async fn create(
    State(api): State<Api>,
    Extension(caller): Extension<Caller>,
    Query(query): Query<InWorkspace>,
    Body(fields): Body<AgentFields>,
) -> ApiResult<(StatusCode, Json<AgentView>)> {
    let workspace = named_workspace(&caller, query.workspace.as_deref())?;
    let active = api.engine.agents(Some(workspace)).await?.len();
    within_plan(&api, workspace, Quota::Agents, "agents", active).await?;
    let agent = api
        .engine
        .create_agent(workspace, fields, &caller.actor())
        .await?;
    Ok((StatusCode::CREATED, Json(one(&api, &caller, agent).await?)))
}

/// `PATCH /agents/{id}` with any of the fields: a new version of each of
/// its system workflows.
pub(crate) async fn update(
    State(api): State<Api>,
    Extension(caller): Extension<Caller>,
    Path(agent): Path<String>,
    Body(fields): Body<AgentFields>,
) -> ApiResult<Json<AgentView>> {
    let agent: AgentId = id(&agent, "agent")?;
    let agent = api
        .engine
        .update_agent(agent, fields, &caller.actor())
        .await?;
    Ok(Json(one(&api, &caller, agent).await?))
}

/// `DELETE /agents/{id}`: archives it.
pub(crate) async fn archive(
    State(api): State<Api>,
    Extension(caller): Extension<Caller>,
    Path(agent): Path<String>,
) -> ApiResult<StatusCode> {
    let agent: AgentId = id(&agent, "agent")?;
    api.engine.archive_agent(agent, &caller.actor()).await?;
    Ok(StatusCode::NO_CONTENT)
}
