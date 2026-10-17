//! A person's chats, with an agent or straight with a tool:
//! `GET /chats?workspace=` (theirs), `POST /chats` and, without an agent,
//! `PATCH /chats/{id}` (another model, effort or mode). A chat is a hidden
//! task, so its messages, thread and runs are the `/tasks/{id}/…` and
//! `/runs/{id}/…` routes, which answer 404 to anyone but its creator
//! ([`crate::workspace_access::guard`]). Creating or switching one needs
//! `run` on its workspace (`require_by_method`); `bypass_permissions` also
//! needs `manage_runtimes` there and `runtimes.allow_bypass`, as on an agent.

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::{Extension, Json};
use serde::{Deserialize, Serialize};
use sun_core::model::{Permission, TaskActivity};
use sun_core::{
    AgentId, ChainSpec, PermissionMode, RunStatus, TaskId, TaskStatus, Timestamp, WorkspaceId,
};
use sun_engine::{Chat, ChatChange};

use crate::Api;
use crate::auth::{self, Caller};
use crate::error::{ApiError, ApiResult};
use crate::hosts::check_bypass_allowed;
use crate::routes::{Body, id};
use crate::workspace_access::{InWorkspace, named_workspace};

/// One chat as the Chat screen lists it.
#[derive(Debug, Serialize)]
pub(crate) struct ChatView {
    id: String,
    workspace_id: String,
    agent_id: Option<AgentId>,
    title: String,
    status: TaskStatus,
    /// Its latest run: `{id, status}`.
    run: Option<ChatRun>,
    /// Its latest message.
    last: Option<TaskActivity>,
    /// While its run is queued: the agent's task keys holding the slots.
    queued_behind: Vec<String>,
    /// Without an agent: the tool it talks to, and how (else `null`).
    runtime: Option<ChainSpec>,
    permission_mode: Option<PermissionMode>,
    created_at: Timestamp,
    updated_at: Timestamp,
}

#[derive(Debug, Serialize)]
struct ChatRun {
    id: String,
    status: RunStatus,
}

impl From<Chat> for ChatView {
    fn from(chat: Chat) -> Self {
        let Chat {
            task,
            run,
            last,
            queued_behind,
            direct,
        } = chat;
        let (runtime, permission_mode) = match direct {
            Some(d) => (Some(d.runtime), d.permission_mode),
            None => (None, None),
        };
        Self {
            id: task.id.to_string(),
            workspace_id: task.workspace_id.to_string(),
            agent_id: task.assignee_agent_id,
            title: task.title,
            status: task.status,
            run: run.map(|r| ChatRun {
                id: r.id.to_string(),
                status: r.status,
            }),
            last,
            queued_behind: queued_behind.iter().map(ToString::to_string).collect(),
            runtime,
            permission_mode,
            created_at: task.created_at,
            updated_at: task.updated_at,
        }
    }
}

/// `GET /chats?workspace=`: the caller's chats there, last active first.
pub(crate) async fn list(
    State(api): State<Api>,
    Extension(caller): Extension<Caller>,
    Query(query): Query<InWorkspace>,
) -> ApiResult<Json<Vec<ChatView>>> {
    let workspace = named_workspace(&caller, query.workspace.as_deref())?;
    let chats = api.engine.chats(workspace, caller.member_id).await?;
    Ok(Json(chats.into_iter().map(ChatView::from).collect()))
}

/// `POST /chats`: `{agent_id}`, or `{workspace_id, runtime, permission_mode?}`.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct NewChat {
    agent_id: Option<AgentId>,
    workspace_id: Option<WorkspaceId>,
    runtime: Option<ChainSpec>,
    permission_mode: Option<PermissionMode>,
}

/// `POST /chats {agent_id}`: a new chat with an active agent of a
/// workspace the caller is on. `{workspace_id, runtime: {tool, credential?,
/// host?, model?, effort?}, permission_mode?}`: one straight with that tool
/// there, checked like an agent's chain entry (`runtime.effort: ...`).
pub(crate) async fn create(
    State(api): State<Api>,
    Extension(caller): Extension<Caller>,
    Body(input): Body<NewChat>,
) -> ApiResult<(StatusCode, Json<ChatView>)> {
    let chat = match input {
        NewChat {
            agent_id: Some(agent),
            workspace_id: None,
            runtime: None,
            permission_mode: None,
        } => {
            let task = api.engine.create_chat(agent, caller.member_id).await?;
            Chat {
                task,
                run: None,
                last: None,
                queued_behind: Vec::new(),
                direct: None,
            }
        }
        NewChat {
            agent_id: None,
            workspace_id: Some(workspace),
            runtime: Some(runtime),
            permission_mode,
        } => {
            check_bypass(&api, &caller, workspace, permission_mode)?;
            let (mode, creator) = (permission_mode, caller.member_id);
            api.engine
                .create_direct_chat(workspace, runtime, mode, creator)
                .await?
        }
        _ => {
            return Err(ApiError::invalid(
                "Pass {agent_id}, or {workspace_id, runtime} for a chat without an agent.",
            ));
        }
    };
    Ok((StatusCode::CREATED, Json(chat.into())))
}

/// `PATCH /chats/{id} {runtime?, permission_mode?}`: a chat without an
/// agent switched to another model, effort or mode; its next message starts
/// a fresh session that reads the thread. Another tool, credential or host,
/// or a reply on its way, is `422`.
pub(crate) async fn update(
    State(api): State<Api>,
    Extension(caller): Extension<Caller>,
    Path(chat): Path<String>,
    Body(change): Body<ChatChange>,
) -> ApiResult<Json<ChatView>> {
    let chat: TaskId = id(&chat, "chat")?;
    if let Some(mode) = change.permission_mode {
        let workspace = api.engine.task(chat).await?.workspace_id;
        check_bypass(&api, &caller, workspace, mode)?;
    }
    let actor = caller.task_actor();
    let chat = api
        .engine
        .update_direct_chat(chat, change, Some(&actor))
        .await?;
    Ok(Json(chat.into()))
}

/// `bypass_permissions` needs `manage_runtimes` on `workspace` and the
/// operator's `runtimes.allow_bypass`, as saving an agent in it does.
fn check_bypass(
    api: &Api,
    caller: &Caller,
    workspace: WorkspaceId,
    mode: Option<PermissionMode>,
) -> ApiResult<()> {
    if mode != Some(PermissionMode::BypassPermissions) {
        return Ok(());
    }
    if !caller.can_in(workspace, Permission::ManageRuntimes) {
        return Err(auth::denied(Permission::ManageRuntimes));
    }
    check_bypass_allowed(api, true)
}
