//! `/catalog` routes: skills and MCP servers. Workflow templates are
//! `/templates`; the web catalog lists both.

use std::collections::BTreeSet;
use std::path::Path as FsPath;

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::{Deserialize, Serialize};
use sun_catalog::{Catalog, EntryFile, Kind, Listing, McpServer, McpTool, detect_stacks};
use sun_core::{Error, McpGrant};
use sun_engine::{MAX_TASK_MCP_GRANTS, SkillPr};

use crate::Api;
use crate::error::{ApiError, ApiResult};
use crate::routes::{Body, id};

#[derive(Deserialize)]
pub(crate) struct RepoFilter {
    /// Absolute path of a local checkout to suggest for.
    repo: Option<String>,
    /// A registered repo whose by-PR skill installs to list.
    repo_id: Option<String>,
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

/// Lists the catalog, ranked for the repo's stack when `?repo=` is given.
pub(crate) async fn list(
    State(api): State<Api>,
    Query(filter): Query<RepoFilter>,
) -> ApiResult<Json<CatalogView>> {
    let stacks = match filter.repo.as_deref().filter(|r| !r.trim().is_empty()) {
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
    let in_use = api.engine.catalog_names_in_use().await?;
    let entries = catalog.list(&stacks, &in_use)?;
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

/// Installs (or updates) one skill or MCP server in the ecosystem (204), or
/// a skill into a repo by PR (202 with its [`SkillPr`]).
pub(crate) async fn install(
    State(api): State<Api>,
    Path(id): Path<String>,
    Body(body): Body<InstallBody>,
) -> ApiResult<Response> {
    let catalog = catalog(&api)?;
    let runtimes = body.runtimes.as_deref();
    let Some(repo_id) = body.repo_id.as_deref() else {
        catalog.install(&id, runtimes)?;
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

/// Uninstalls a skill or MCP server from the ecosystem.
pub(crate) async fn uninstall(
    State(api): State<Api>,
    Path(id): Path<String>,
) -> ApiResult<StatusCode> {
    catalog(&api)?.uninstall(&id)?;
    Ok(StatusCode::NO_CONTENT)
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

/// Values of the ecosystem secrets `server` needs.
async fn mcp_secrets(api: &Api, server: &McpServer) -> ApiResult<Vec<(String, String)>> {
    Ok(api.engine.mcp_secrets(&server.secret_names()).await?)
}

/// Every registered MCP server (catalog installs and custom ones).
pub(crate) async fn mcp_servers(State(api): State<Api>) -> ApiResult<Json<Vec<McpServer>>> {
    Ok(Json(catalog(&api)?.mcp_servers()?))
}

/// Registers an MCP server (admin): validates it, connects once to list its
/// tools, then saves it.
pub(crate) async fn add_mcp_server(
    State(api): State<Api>,
    Body(server): Body<McpServer>,
) -> ApiResult<(StatusCode, Json<McpServerView>)> {
    let secrets = mcp_secrets(&api, &server).await?;
    let tools = catalog(&api)?
        .register_mcp_server(&server, secrets)
        .await
        .map_err(unreachable(&server.name))?;
    Ok((StatusCode::CREATED, Json(McpServerView { server, tools })))
}

/// The tools a registered MCP server offers now (connects to it).
pub(crate) async fn mcp_server_tools(
    State(api): State<Api>,
    Path(name): Path<String>,
) -> ApiResult<Json<Vec<McpTool>>> {
    let catalog = catalog(&api)?;
    let server = catalog.mcp_server(&name)?;
    let secrets = mcp_secrets(&api, &server).await?;
    let tools = catalog
        .mcp_tools(&server, secrets)
        .await
        .map_err(unreachable(&name))?;
    Ok(Json(tools))
}

/// Unregisters an MCP server (admin); tasks granting it lose it at their
/// next session.
pub(crate) async fn remove_mcp_server(
    State(api): State<Api>,
    Path(name): Path<String>,
) -> ApiResult<StatusCode> {
    catalog(&api)?.remove_mcp_server(&name)?;
    Ok(StatusCode::NO_CONTENT)
}

/// The MCP servers (and tools) a task's agent sessions get.
pub(crate) async fn task_mcp(
    State(api): State<Api>,
    Path(task): Path<String>,
) -> ApiResult<Json<Vec<McpGrant>>> {
    Ok(Json(api.engine.task_mcp(id(&task, "task")?).await?))
}

/// Replaces a task's MCP grants; every server must be registered.
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
    let registered = catalog(&api)?.mcp_servers()?;
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
