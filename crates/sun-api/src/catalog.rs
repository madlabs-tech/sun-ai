//! `/catalog` routes: skills and MCP servers, installed per workspace
//! (`?workspace=`; a deployment owner may leave it out: every workspace's
//! reads, Main's changes). The files are the deployment's, shared on disk
//! (`sun_catalog`); which workspaces have an entry is the store's. Workflow
//! templates are `/templates`; the web catalog lists both.

use std::collections::BTreeSet;
use std::path::Path as FsPath;

use axum::Extension;
use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::{Deserialize, Serialize};
use sun_catalog::{Catalog, EntryFile, Kind, Listing, McpServer, McpTool, detect_stacks};
use sun_core::{Error, McpGrant, WorkspaceId};
use sun_engine::{MAX_TASK_MCP_GRANTS, SkillPr, WorkspaceItem};

use crate::Api;
use crate::auth::Caller;
use crate::error::{ApiError, ApiResult};
use crate::routes::{Body, id};
use crate::workspace_access::{InWorkspace, listed_workspace, named_workspace};

#[derive(Deserialize)]
pub(crate) struct RepoFilter {
    /// Absolute path of a local checkout to suggest for.
    repo: Option<String>,
    /// A registered repo whose by-PR skill installs to list.
    repo_id: Option<String>,
    /// The workspace whose installs to show.
    workspace: Option<String>,
}

#[derive(Serialize)]
pub(crate) struct CatalogView {
    /// Stack tags detected in `repo` (empty without one).
    stacks: Vec<String>,
    /// Every entry, best fit first.
    entries: Vec<Listing>,
    /// Suggestions this ecosystem dismissed (catalog and template ids).
    dismissed: BTreeSet<String>,
    /// With `?repo_id=`: the skills installed into that repo by PR.
    #[serde(skip_serializing_if = "Option::is_none")]
    repo_installs: Option<Vec<SkillPr>>,
}

fn catalog(api: &Api) -> ApiResult<&Catalog> {
    api.catalog.as_deref().ok_or_else(|| {
        ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "unavailable",
            "This kernel runs without a catalog.",
        )
    })
}

/// Lists the catalog with the workspace's installs, ranked for the repo's
/// stack when `?repo=` (a path on the kernel's machine: deployment owners
/// only) is given.
pub(crate) async fn list(
    State(api): State<Api>,
    Extension(caller): Extension<Caller>,
    Query(filter): Query<RepoFilter>,
) -> ApiResult<Json<CatalogView>> {
    let workspace = listed_workspace(&caller, filter.workspace.as_deref())?;
    let stacks = match filter.repo.as_deref().filter(|r| !r.trim().is_empty()) {
        Some(_) if !caller.is_owner => {
            return Err(ApiError::invalid(
                "repo: only a deployment owner may name a folder on the kernel's machine; \
                 use repo_id.",
            ));
        }
        Some(repo) => {
            let path = FsPath::new(repo.trim());
            if !path.is_absolute() || !path.is_dir() {
                return Err(ApiError::invalid(format!(
                    "`{repo}` is not a folder on the kernel's machine. Give the absolute path of a checkout."
                )));
            }
            detect_stacks(path)?
        }
        None => Vec::new(),
    };
    let catalog = catalog(&api)?;
    let in_use = api.engine.catalog_names_in_use(workspace).await?;
    let installs = match workspace {
        Some(workspace) => Some(api.engine.catalog_installs(workspace).await?),
        None => None,
    };
    let entries = catalog.list(&stacks, &in_use, installs.as_deref())?;
    let repo_installs = match filter.repo_id.as_deref() {
        Some(repo_id) => {
            let repo_id = id(repo_id, "repo")?;
            let mut installs = Vec::new();
            for entry in entries.iter().filter(|l| l.entry.kind == Kind::Skill) {
                installs.extend(api.engine.skill_pr(repo_id, &entry.entry.id).await?);
            }
            Some(installs)
        }
        None => None,
    };
    Ok(Json(CatalogView {
        stacks,
        entries,
        dismissed: catalog.dismissed()?,
        repo_installs,
    }))
}

/// Dismisses one suggestion for the whole ecosystem.
pub(crate) async fn dismiss(
    State(api): State<Api>,
    Path(id): Path<String>,
) -> ApiResult<StatusCode> {
    catalog(&api)?.dismiss(&id, true)?;
    Ok(StatusCode::NO_CONTENT)
}

/// Shows a dismissed suggestion again.
pub(crate) async fn restore(
    State(api): State<Api>,
    Path(id): Path<String>,
) -> ApiResult<StatusCode> {
    catalog(&api)?.dismiss(&id, false)?;
    Ok(StatusCode::NO_CONTENT)
}

/// The entry's full text, shown before install.
pub(crate) async fn files(
    State(api): State<Api>,
    Path(id): Path<String>,
) -> ApiResult<Json<Vec<EntryFile>>> {
    Ok(Json(catalog(&api)?.files(&id)?))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct InstallBody {
    /// Skill: the runtime adapter ids that get it; absent = every runtime.
    #[serde(default)]
    runtimes: Option<Vec<String>>,
    /// Skill: install into this repo by PR instead of the ecosystem store.
    #[serde(default)]
    repo_id: Option<String>,
}

/// Installs (or updates) one skill or MCP server in the workspace (204;
/// the files on disk are shared, so an update is every workspace's), or a
/// skill into a repo by PR (202 with its [`SkillPr`]). Audited.
pub(crate) async fn install(
    State(api): State<Api>,
    Extension(caller): Extension<Caller>,
    Path(id): Path<String>,
    Query(query): Query<InWorkspace>,
    Body(body): Body<InstallBody>,
) -> ApiResult<Response> {
    let catalog = catalog(&api)?;
    let runtimes = body.runtimes.as_deref();
    let Some(repo_id) = body.repo_id.as_deref() else {
        let workspace = named_workspace(&caller, query.workspace.as_deref())?;
        catalog.install(&id, runtimes)?;
        let item = WorkspaceItem::CatalogInstall(&id);
        api.engine
            .import_to_workspace(workspace, item, &caller.actor())
            .await?;
        return Ok(StatusCode::NO_CONTENT.into_response());
    };
    let repo_id = crate::routes::id(repo_id, "repo")?;
    let (version, files) = catalog.skill_bundle(&id, runtimes)?;
    let pr = api
        .engine
        .install_skill_pr(repo_id, &id, &version, &files, runtimes)
        .await?;
    Ok((StatusCode::ACCEPTED, Json(pr)).into_response())
}

/// A skill's install into a repo by PR, with where its PR stands.
pub(crate) async fn repo_install(
    State(api): State<Api>,
    Path((id, repo_id)): Path<(String, String)>,
) -> ApiResult<Json<SkillPr>> {
    let repo_id = crate::routes::id(&repo_id, "repo")?;
    match api.engine.skill_pr(repo_id, &id).await? {
        Some(pr) => Ok(Json(pr)),
        None => Err(Error::NotFound(format!("an install of `{id}` into repo {repo_id}")).into()),
    }
}

/// Uninstalls a skill or MCP server from the workspace (404 when it has
/// none); its files go when no workspace has it any more. Audited.
pub(crate) async fn uninstall(
    State(api): State<Api>,
    Extension(caller): Extension<Caller>,
    Path(id): Path<String>,
    Query(query): Query<InWorkspace>,
) -> ApiResult<StatusCode> {
    let workspace = named_workspace(&caller, query.workspace.as_deref())?;
    uninstall_from(&api, &caller, workspace, &id).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Removes `workspace`'s install of `id`, and the shared files with the
/// last one.
async fn uninstall_from(
    api: &Api,
    caller: &Caller,
    workspace: WorkspaceId,
    id: &str,
) -> ApiResult<()> {
    let catalog = catalog(api)?;
    let item = WorkspaceItem::CatalogInstall(id);
    if api
        .engine
        .detach_from_workspace(workspace, item, &caller.actor())
        .await?
    {
        match catalog.uninstall(id) {
            Ok(()) | Err(Error::NotFound(_)) => {}
            Err(e) => return Err(e.into()),
        }
    }
    Ok(())
}

/// One registered MCP server with the tools it offers right now.
#[derive(Serialize)]
pub(crate) struct McpServerView {
    server: McpServer,
    tools: Vec<McpTool>,
}

/// An unreachable or misbehaving server is the caller's input problem: 422
/// in plain words, not 503.
fn unreachable(name: &str) -> impl Fn(Error) -> ApiError + '_ {
    move |e| match e {
        Error::Unavailable(detail) | Error::Protocol(detail) => ApiError::invalid(format!(
            "Sun could not connect to {name} and list its tools: {detail}"
        )),
        other => other.into(),
    }
}

/// Values of the secrets `server` needs, from `workspace`'s.
async fn mcp_secrets(
    api: &Api,
    workspace: WorkspaceId,
    server: &McpServer,
) -> ApiResult<Vec<(String, String)>> {
    Ok(api
        .engine
        .mcp_secrets(workspace, &server.secret_names())
        .await?)
}

/// The registered MCP servers `workspace` has: its catalog installs and its
/// custom servers.
pub(crate) async fn mcp_servers_in(api: &Api, workspace: WorkspaceId) -> ApiResult<Vec<McpServer>> {
    let installs = api.engine.catalog_installs(workspace).await?;
    Ok(catalog(api)?.mcp_servers_in(workspace, &installs)?)
}

/// `workspace`'s MCP server `name`; `NotFound` when it has none by that name.
pub(crate) async fn mcp_server_in(
    api: &Api,
    workspace: WorkspaceId,
    name: &str,
) -> ApiResult<McpServer> {
    mcp_servers_in(api, workspace)
        .await?
        .into_iter()
        .find(|s| s.name == name)
        .ok_or_else(|| Error::NotFound(format!("mcp server `{name}` is not registered")).into())
}

/// `GET /mcp_servers?workspace=`: the workspace's MCP servers (catalog
/// installs and custom ones); a deployment owner may leave it out for
/// every registered one.
pub(crate) async fn mcp_servers(
    State(api): State<Api>,
    Extension(caller): Extension<Caller>,
    Query(query): Query<InWorkspace>,
) -> ApiResult<Json<Vec<McpServer>>> {
    Ok(Json(
        match listed_workspace(&caller, query.workspace.as_deref())? {
            Some(workspace) => mcp_servers_in(&api, workspace).await?,
            None => catalog(&api)?.mcp_servers()?,
        },
    ))
}

/// `POST /mcp_servers?workspace=` (`manage_plugins` there): registers a
/// custom MCP server in the workspace: validates it, connects once with the
/// workspace's secrets to list its tools, then saves it. Names are unique
/// in the deployment.
pub(crate) async fn add_mcp_server(
    State(api): State<Api>,
    Extension(caller): Extension<Caller>,
    Query(query): Query<InWorkspace>,
    Body(server): Body<McpServer>,
) -> ApiResult<(StatusCode, Json<McpServerView>)> {
    let workspace = named_workspace(&caller, query.workspace.as_deref())?;
    let server = McpServer {
        workspace_id: Some(workspace),
        ..server
    };
    let secrets = mcp_secrets(&api, workspace, &server).await?;
    let tools = catalog(&api)?
        .register_mcp_server(&server, secrets)
        .await
        .map_err(unreachable(&server.name))?;
    Ok((StatusCode::CREATED, Json(McpServerView { server, tools })))
}

/// `GET /mcp_servers/{name}/tools?workspace=`: the tools the workspace's MCP
/// server offers now (connects to it with the workspace's secrets).
pub(crate) async fn mcp_server_tools(
    State(api): State<Api>,
    Extension(caller): Extension<Caller>,
    Path(name): Path<String>,
    Query(query): Query<InWorkspace>,
) -> ApiResult<Json<Vec<McpTool>>> {
    let workspace = named_workspace(&caller, query.workspace.as_deref())?;
    let server = mcp_server_in(&api, workspace, &name).await?;
    let secrets = mcp_secrets(&api, workspace, &server).await?;
    let tools = catalog(&api)?
        .mcp_tools(&server, secrets)
        .await
        .map_err(unreachable(&name))?;
    Ok(Json(tools))
}

/// `DELETE /mcp_servers/{name}?workspace=` (`manage_plugins` there):
/// unregisters the workspace's custom MCP server, or uninstalls its catalog
/// one; tasks granting it lose it at their next session.
pub(crate) async fn remove_mcp_server(
    State(api): State<Api>,
    Extension(caller): Extension<Caller>,
    Path(name): Path<String>,
    Query(query): Query<InWorkspace>,
) -> ApiResult<StatusCode> {
    let workspace = named_workspace(&caller, query.workspace.as_deref())?;
    let server = mcp_server_in(&api, workspace, &name).await?;
    let catalog = catalog(&api)?;
    match catalog.is_catalog_install(&server.name)? {
        true => uninstall_from(&api, &caller, workspace, &server.name).await?,
        false => catalog.remove_mcp_server(&server.name)?,
    }
    Ok(StatusCode::NO_CONTENT)
}

/// The MCP servers (and tools) a task's agent sessions get.
pub(crate) async fn task_mcp(
    State(api): State<Api>,
    Path(task): Path<String>,
) -> ApiResult<Json<Vec<McpGrant>>> {
    Ok(Json(api.engine.task_mcp(id(&task, "task")?).await?))
}

/// Replaces a task's MCP grants; every server must be one of the task's
/// workspace's.
pub(crate) async fn set_task_mcp(
    State(api): State<Api>,
    Path(task): Path<String>,
    Body(grants): Body<Vec<McpGrant>>,
) -> ApiResult<Json<Vec<McpGrant>>> {
    let task_id = id(&task, "task")?;
    if grants.len() > MAX_TASK_MCP_GRANTS {
        return Err(ApiError::invalid(format!(
            "A task can use at most {MAX_TASK_MCP_GRANTS} MCP servers."
        )));
    }
    let workspace = api.engine.workspace_of_task(task_id).await?;
    let registered = mcp_servers_in(&api, workspace).await?;
    if let Some(grant) = grants
        .iter()
        .find(|g| !registered.iter().any(|s| s.name == g.server))
    {
        return Err(ApiError::invalid(format!(
            "No MCP server named {} is registered. Add it first.",
            grant.server
        )));
    }
    api.engine.set_task_mcp(task_id, &grants).await?;
    Ok(Json(grants))
}
