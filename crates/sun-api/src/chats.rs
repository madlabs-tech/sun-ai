//! A person's chats with agents: `GET /chats?workspace=` (theirs) and
//! `POST /chats {agent_id}`. A chat is a hidden task, so its messages,
//! thread and runs are the `/tasks/{id}/…` and `/runs/{id}/…` routes, which
//! answer 404 to anyone but its creator
//! ([`crate::workspace_access::guard`]). Creating one needs `run` on the
//! agent's workspace (`require_by_method`).

use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::{Extension, Json};
use serde::{Deserialize, Serialize};
use sun_core::model::TaskActivity;
use sun_core::{AgentId, RunStatus, TaskStatus, Timestamp};
use sun_engine::Chat;

use crate::Api;
use crate::auth::Caller;
use crate::error::ApiResult;
use crate::routes::Body;
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
        } = chat;
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

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct NewChat {
    agent_id: AgentId,
}

/// `POST /chats {agent_id}`: a new chat with an active agent of a
/// workspace the caller is on.
pub(crate) async fn create(
    State(api): State<Api>,
    Extension(caller): Extension<Caller>,
    Body(input): Body<NewChat>,
) -> ApiResult<(StatusCode, Json<ChatView>)> {
    let task = api
        .engine
        .create_chat(input.agent_id, caller.member_id)
        .await?;
    let chat = Chat {
        task,
        run: None,
        last: None,
        queued_behind: Vec::new(),
    };
    Ok((StatusCode::CREATED, Json(chat.into())))
}
