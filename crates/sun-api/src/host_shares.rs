//! Shared hosts (BLO-197): a deployment owner shares a host with groups
//! (`/hosts/{id}/shares`), and `GET /hosts/attachable` lists the hosts a
//! caller may attach to a workspace, by the one rule `attach_host` applies
//! ([`sun_engine::attach_path`]). The kernel's own host (`sun serve
//! --with-host`) is shared only with `confirm_kernel_host: true`: its
//! sessions run next to the kernel and can read `SUN_MASTER_KEY`.

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use serde_json::{Value, json};
use sun_core::{GroupId, HostId};

use crate::auth::Caller;
use crate::error::{ApiError, ApiResult};
use crate::routes::id;
use crate::workspace_access::{InWorkspace, named_workspace};
use crate::{Api, wire};

/// The hosts shared with a group `caller` is in (none for a deployment
/// owner, who may attach any host anyway).
pub(crate) async fn shared_with(api: &Api, caller: &Caller) -> ApiResult<Vec<HostId>> {
    match caller.member_id {
        Some(member) if !caller.is_owner => Ok(api.engine.hosts_shared_with(member).await?),
        _ => Ok(Vec::new()),
    }
}

/// `GET /api/v1/hosts/attachable?workspace=` (`manage_runtimes` there): every
/// host the caller may attach to the workspace, each with why (`via`:
/// `owner`, `enrolled` or `share`), whether it is the kernel's own host
/// (`kernel`), whether the workspace already has it (`attached`), the
/// workspaces it serves that the caller sees, its enroller, OS and status.
/// No other workspace's runs, tasks or sessions.
pub(crate) async fn attachable(
    State(api): State<Api>,
    axum::Extension(caller): axum::Extension<Caller>,
    Query(query): Query<InWorkspace>,
) -> ApiResult<Json<Vec<Value>>> {
    let workspace = named_workspace(&caller, query.workspace.as_deref())?;
    let shared = shared_with(&api, &caller).await?;
    let kernel = api.engine.kernel_host().await?;
    let attached: Vec<HostId> = api
        .engine
        .hosts(Some(workspace))
        .await?
        .into_iter()
        .map(|h| h.id)
        .collect();
    let online = api.hosts.online();
    let mut views = Vec::new();
    for host in api.engine.hosts(None).await? {
        let Some(via) = sun_engine::attach_path(&host, caller.member_id, caller.is_owner, &shared)
        else {
            continue;
        };
        let item = sun_engine::WorkspaceItem::Host(host.id);
        let mut workspace_ids = api.engine.workspaces_of(item).await?;
        workspace_ids.retain(|w| caller.sees(*w));
        let caps = &host.capabilities;
        views.push(json!({
            "id": host.id,
            "name": host.name,
            "via": via,
            "kernel": kernel == Some(host.id),
            "attached": attached.contains(&host.id),
            "workspace_ids": workspace_ids,
            "enrolled_by": host.enrolled_by,
            "capabilities": {
                "os": caps.get("os").cloned().unwrap_or(Value::from("")),
                "arch": caps.get("arch").cloned().unwrap_or(Value::from("")),
            },
            "status": if online.contains(&host.id) { "online" } else { "offline" },
            "last_seen_at": api.hosts.last_seen(host.id)
                .unwrap_or_else(|| wire::created_at(&host.id.to_string())),
        }));
    }
    Ok(Json(views))
}

/// `GET /api/v1/hosts/{id}/shares` (deployment owner): the groups the host
/// is shared with, oldest first, and whether it is the kernel's own host.
pub(crate) async fn list(
    State(api): State<Api>,
    Path(host): Path<String>,
) -> ApiResult<Json<Value>> {
    let host: HostId = id(&host, "host")?;
    let shares = api.engine.host_shares(host).await?;
    let kernel = api.engine.kernel_host().await? == Some(host);
    Ok(Json(json!({ "kernel": kernel, "shares": shares })))
}

/// Body of `PUT /hosts/{id}/shares/{group}`; may be empty.
#[derive(Default, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ShareInput {
    /// Required to share the kernel's own host.
    #[serde(default)]
    confirm_kernel_host: bool,
}

/// `PUT /api/v1/hosts/{id}/shares/{group} {confirm_kernel_host?}` (deployment
/// owner): members of the group may attach the host where they manage
/// runtimes. 204, also when it already was; 404 for a missing host or
/// group; 409 for the kernel's own host without `confirm_kernel_host:
/// true`. Audited.
pub(crate) async fn put(
    State(api): State<Api>,
    axum::Extension(caller): axum::Extension<Caller>,
    Path((host, group)): Path<(String, String)>,
    body: axum::body::Bytes,
) -> ApiResult<StatusCode> {
    let host: HostId = id(&host, "host")?;
    let group: GroupId = id(&group, "group")?;
    let input: ShareInput = match body.iter().all(u8::is_ascii_whitespace) {
        true => ShareInput::default(),
        false => serde_json::from_slice(&body)
            .map_err(|e| ApiError::invalid(format!("The request body is not valid: {e}")))?,
    };
    // A missing host is 404 before the kernel-host question.
    api.engine.host(host).await?;
    if api.engine.kernel_host().await? == Some(host) && !input.confirm_kernel_host {
        return Err(ApiError::conflict(
            "This is the kernel's own host: agent sessions and terminals on it run next to the \
             kernel and can read SUN_MASTER_KEY, which decrypts every stored credential. Share a \
             separate host instead, or send confirm_kernel_host: true.",
        ));
    }
    api.engine
        .share_host(host, group, caller.member_id, &caller.actor())
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

/// `DELETE /api/v1/hosts/{id}/shares/{group}` (deployment owner): workspaces
/// that had the host only through this share get no new runs on it;
/// running sessions finish. 204; 404 when it wasn't shared with the group.
/// Audited.
pub(crate) async fn delete(
    State(api): State<Api>,
    axum::Extension(caller): axum::Extension<Caller>,
    Path((host, group)): Path<(String, String)>,
) -> ApiResult<StatusCode> {
    let host: HostId = id(&host, "host")?;
    let group: GroupId = id(&group, "group")?;
    api.engine
        .unshare_host(host, group, &caller.actor())
        .await?;
    Ok(StatusCode::NO_CONTENT)
}
