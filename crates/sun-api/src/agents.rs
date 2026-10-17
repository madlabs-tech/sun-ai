//! Agents: `GET|POST /agents?workspace=`, `PATCH|DELETE /agents/{id}`
//! (DELETE archives). An agent belongs to one workspace: the guard keeps
//! everyone but its members off it, any member reads, and writes need
//! `manage_runtimes` there (the router). Only a deployment owner may leave
//! out `?workspace=` (every workspace's list; a new agent goes to Main).
//! `permission_mode: bypass_permissions` also needs `runtimes.allow_bypass`.

use std::collections::{BTreeMap, HashMap};

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::{Extension, Json};
use serde::Serialize;
use sun_core::model::Agent;
use sun_core::{AgentId, ChainEntry, HostId, HostLink, PermissionMode, Timestamp, WorkspaceId};
use sun_engine::AgentFields;

use crate::Api;
use crate::auth::Caller;
use crate::error::ApiResult;
use crate::hosts::{check_bypass_allowed, online_adapters, readiness_of};
use crate::routes::{Body, id};
use crate::workspace_access::{InWorkspace, listed_workspace, named_workspace};

/// `Agent` in `types.ts`: the row, whether it can run now, and its open tasks.
#[derive(Debug, Serialize)]
pub(crate) struct AgentView {
    id: AgentId,
    workspace_id: WorkspaceId,
    name: String,
    description: String,
    instructions: String,
    runtime_chain: Vec<ChainEntry>,
    skill_ids: Vec<String>,
    /// `null`: inherits.
    permission_mode: Option<PermissionMode>,
    /// Its own mode, else the first chain runtime's `runtime_policy` row;
    /// `null`: the host's `sun.yaml` default.
    effective_permission_mode: Option<String>,
    created_at: Timestamp,
    archived_at: Option<Timestamp>,
    /// Some runtime of the chain is logged in on an online host.
    ready: bool,
    /// Why not, per runtime of the chain; `null` when ready.
    ready_reason: Option<String>,
    /// Its tasks that aren't Done or Cancelled, on workspaces the caller sees.
    task_count: u32,
}

/// The view of `agent`, its readiness read from `online` and its inherited
/// mode from `modes` (`runtime_policy`).
async fn view(
    api: &Api,
    agent: Agent,
    online: &BTreeMap<String, (bool, bool)>,
    modes: &HashMap<String, String>,
    task_count: u32,
) -> ApiResult<AgentView> {
    let mut reasons = Vec::new();
    let mut ready = false;
    for entry in &agent.runtime_chain {
        let name = entry.name();
        // A pinned entry runs only on its host.
        let pin_online = entry.host().map(|pin| {
            pin.parse::<HostId>()
                .is_ok_and(|host| api.hosts.is_online(host))
        });
        if pin_online == Some(false) {
            reasons.push(format!("{name}: its host is offline"));
            continue;
        }
        let reason = match readiness_of(api, agent.workspace_id, name.to_owned(), online).await {
            Ok(r) if r.installed && r.logged_in => {
                ready = true;
                break;
            }
            Ok(r) => r.detail.unwrap_or_else(|| "not ready".into()),
            Err(_) => "no host reports it".into(),
        };
        reasons.push(format!("{name}: {reason}"));
    }
    let effective_permission_mode = match (agent.permission_mode, agent.runtime_chain.first()) {
        (Some(mode), _) => Some(mode.as_str().to_owned()),
        (None, Some(first)) => {
            let name = first.name();
            // The tool the engine resolves the name to, as placement does.
            let adapter = match api.engine.runtime(agent.workspace_id, name).await {
                Ok(row) => row.adapter,
                Err(_) => name.to_owned(),
            };
            modes.get(&adapter).cloned()
        }
        (None, None) => None,
    };
    Ok(AgentView {
        id: agent.id,
        workspace_id: agent.workspace_id,
        name: agent.name,
        description: agent.description,
        instructions: agent.instructions,
        runtime_chain: agent.runtime_chain,
        skill_ids: agent.skill_ids,
        permission_mode: agent.permission_mode,
        effective_permission_mode,
        created_at: agent.created_at,
        archived_at: agent.archived_at,
        ready,
        ready_reason: (!ready).then(|| reasons.join("; ")),
        task_count,
    })
}

/// One agent's view for `caller`, read now.
async fn one(api: &Api, caller: &Caller, agent: Agent) -> ApiResult<AgentView> {
    let online = online_adapters(api, Some(agent.workspace_id)).await?;
    let modes = api.engine.permission_modes().await?.into_iter().collect();
    let counts = api.engine.agent_task_counts(|w| caller.sees(w)).await?;
    let count = counts.get(&agent.id).copied().unwrap_or(0);
    view(api, agent, &online, &modes, count).await
}

/// The workspace's active agents, by name.
pub(crate) async fn list(
    State(api): State<Api>,
    Extension(caller): Extension<Caller>,
    Query(query): Query<InWorkspace>,
) -> ApiResult<Json<Vec<AgentView>>> {
    let workspace = listed_workspace(&caller, query.workspace.as_deref())?;
    let online = online_adapters(&api, workspace).await?;
    let modes = api.engine.permission_modes().await?.into_iter().collect();
    let counts = api.engine.agent_task_counts(|w| caller.sees(w)).await?;
    let mut out = Vec::new();
    for agent in api.engine.agents(workspace).await? {
        let count = counts.get(&agent.id).copied().unwrap_or(0);
        out.push(view(&api, agent, &online, &modes, count).await?);
    }
    Ok(Json(out))
}

/// `POST /agents?workspace= {name, runtime_chain, description?,
/// instructions?, skill_ids?, permission_mode?}`. A chain entry is a runtime name or
/// `{tool, credential?, host?, model?, effort?}`; an unknown credential, a
/// host without the tool, or an effort outside the host's catalog is
/// `422 invalid_input` naming the field (`runtime_chain[1].effort: ...`).
pub(crate) async fn create(
    State(api): State<Api>,
    Extension(caller): Extension<Caller>,
    Query(query): Query<InWorkspace>,
    Body(fields): Body<AgentFields>,
) -> ApiResult<(StatusCode, Json<AgentView>)> {
    let workspace = named_workspace(&caller, query.workspace.as_deref())?;
    check_bypass_allowed(&api, bypasses(&fields))?;
    let plan = api.entitlements.for_workspace(workspace).await;
    let agent = api
        .engine
        .create_agent(workspace, fields, &caller.actor(), &plan)
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
    check_bypass_allowed(&api, bypasses(&fields))?;
    let agent = api
        .engine
        .update_agent(agent, fields, &caller.actor())
        .await?;
    Ok(Json(one(&api, &caller, agent).await?))
}

fn bypasses(fields: &AgentFields) -> bool {
    fields.permission_mode == Some(Some(PermissionMode::BypassPermissions))
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
