//! Workspace access: the one guard that keeps a caller off workspaces they
//! aren't a member of. A workspace is visible to its members and to
//! deployment owners ([`sun_engine::Access::workspaces`]).
//!
//! [`guard`] (a **Chain of Responsibility** link after `authenticate`) finds
//! every workspace-scoped thing a request names, resolves where it lives with
//! [`sun_engine::Engine::reach`], and answers 404, never 403, when any of it
//! is hidden or doesn't exist. It stores where the named things live
//! ([`NamedWorkspaces`]) for [`crate::auth::require`], which checks the
//! caller's role in each. It reads:
//! - path ids: `/workspaces/{id}`, `/tasks/{id}/*` (a key there is already
//!   the task's id: [`crate::tasks::key_paths`]), `/runs/{id}/*`,
//!   `/workflows/{id}/*`, `/knowledge/sources/{id}/*`, `/knowledge/docs/{id}`,
//!   `/subscriptions/{id}/*`,
//!   `/artifacts/{id}`, `/evals/{id}`, `/outbox/{id}/resolve`, `/projects/{id}`,
//!   `/repos/{id}`, `/vcs/connections/{id}`, `/agents/{id}`, `/memory/{id}/*`,
//!   `/corrections/{id}/*`, `/notes/{id}/*`, `/secrets/workspace/{id}/*`, and
//!   any `{repo_id}`, `{source_id}` or `{connection_id}`
//!   (so a knowledge import needs both workspaces);
//! - `?workspace=` (or Main for the routes that default to it), `?run_id=` and
//!   `?repo_id=`;
//! - JSON body keys anywhere in the body: `workspace_id`, `workflow_id`,
//!   `task_id(s)`, `repo_id(s)`, `project_id`, `set_id`, an eval `pick` and a
//!   task's `assignee` (a workflow or an agent).
//!
//! Lists without `?workspace=` filter themselves with [`Caller::sees`], or
//! need one from anyone but a deployment owner ([`listed_workspace`]).

use axum::extract::{FromRequestParts, MatchedPath, Query, RawPathParams, Request, State};
use axum::http::{Method, StatusCode, header};
use axum::middleware::Next;
use axum::response::Response;
use serde_json::Value;
use sun_core::{
    AgentId, ArtifactId, CorrectionId, Error, EvalRunId, EvalSetId, MemoryItemId, NoteId, OutboxId,
    ProjectId, RepoId, RunId, TaskId, VcsConnectionId, WorkflowId, WorkspaceId,
};
use sun_engine::{OnWorkspace, Pick, Reach, WorkspaceItem};

use crate::Api;
use crate::auth::Caller;
use crate::error::{ApiError, ApiResult};

/// Bodies the guard reads; larger ones are refused (no route takes one,
/// except doc uploads, which name their source in the path).
const MAX_SCANNED_BODY: usize = 2 * 1024 * 1024;
/// Doc uploads carry base64 files and no ids: not scanned.
const UPLOAD_PATH: &str = "/knowledge/sources/{id}/docs";
/// Routes that act on Main when no workspace is named.
const DEFAULTS_TO_MAIN: &[(Method, &str)] = &[
    (Method::POST, "/workflows"),
    (Method::POST, "/templates/{id}/install"),
    (Method::POST, "/knowledge/sources"),
    (Method::GET, "/knowledge/search"),
    (Method::DELETE, "/knowledge/sources/{id}"),
    (Method::GET, "/entitlement"),
];

/// The query parameters that name something on a workspace.
#[derive(serde::Deserialize)]
struct NamedQuery {
    workspace: Option<String>,
    /// `GET /usage?run_id=`.
    run_id: Option<String>,
    /// `GET /catalog?repo_id=`.
    repo_id: Option<String>,
}

/// Where the things a request names live, for a caller who isn't a
/// deployment owner: one entry per thing, each the workspaces (the caller's
/// own) any of which may grant the permission (a knowledge source on several
/// workspaces); an eval over many tasks adds one entry per workspace.
#[derive(Debug, Clone, Default)]
pub(crate) struct NamedWorkspaces(pub(crate) Vec<Vec<WorkspaceId>>);

/// A workspace-scoped thing named by a request.
enum Named {
    Workspace(WorkspaceId),
    Task(TaskId),
    Run(RunId),
    Workflow(WorkflowId),
    Repo(RepoId),
    Source(String),
    Doc(String),
    Artifact(ArtifactId),
    Eval(EvalRunId),
    EvalSet(EvalSetId),
    Pick(Pick),
    Outbox(OutboxId),
    Project(ProjectId),
    VcsConnection(VcsConnectionId),
    Agent(AgentId),
    MemoryItem(MemoryItemId),
    Correction(CorrectionId),
    Note(NoteId),
}

impl Named {
    fn on_workspace(&self) -> OnWorkspace<'_> {
        match self {
            Named::Workspace(id) => OnWorkspace::Workspace(*id),
            Named::Task(id) => OnWorkspace::Task(*id),
            Named::Run(id) => OnWorkspace::Run(*id),
            Named::Workflow(id) => OnWorkspace::Workflow(*id),
            Named::Repo(id) => OnWorkspace::Repo(*id),
            Named::Source(id) => OnWorkspace::Item(WorkspaceItem::KnowledgeSource(id)),
            Named::Doc(id) => OnWorkspace::Doc(id),
            Named::Artifact(id) => OnWorkspace::Artifact(*id),
            Named::Eval(id) => OnWorkspace::Eval(*id),
            Named::EvalSet(id) => OnWorkspace::EvalSet(*id),
            Named::Pick(pick) => OnWorkspace::Pick(pick),
            Named::Outbox(id) => OnWorkspace::Outbox(*id),
            Named::Project(id) => OnWorkspace::Project(*id),
            Named::VcsConnection(id) => OnWorkspace::VcsConnection(*id),
            Named::Agent(id) => OnWorkspace::Agent(*id),
            Named::MemoryItem(id) => OnWorkspace::MemoryItem(*id),
            Named::Correction(id) => OnWorkspace::Correction(*id),
            Named::Note(id) => OnWorkspace::Note(*id),
        }
    }
}

/// The answer for a hidden workspace: the same as a missing one.
pub(crate) fn hidden() -> ApiError {
    ApiError::new(StatusCode::NOT_FOUND, "not_found", "Not found.")
}

/// Middleware: 404 when the request names anything that doesn't exist or is
/// on a workspace the caller can't see, else stores [`NamedWorkspaces`].
/// Malformed ids name nothing (the handler answers 422). Deployment owners
/// pass without a lookup.
pub(crate) async fn guard(
    State(api): State<Api>,
    request: Request,
    next: Next,
) -> ApiResult<Response> {
    let restricted = request
        .extensions()
        .get::<Caller>()
        .is_some_and(|c| c.workspaces.is_some());
    if !restricted {
        return Ok(next.run(request).await);
    }
    let (mut parts, body) = request.into_parts();
    let path = parts
        .extensions
        .get::<MatchedPath>()
        .map(|p| p.as_str())
        .map(|p| p.strip_prefix("/api/v1").unwrap_or(p).to_owned())
        .unwrap_or_default();
    let mut named = Vec::new();
    if let Ok(params) = RawPathParams::from_request_parts(&mut parts, &()).await {
        for (key, value) in &params {
            from_path(&path, key, value, &mut named);
        }
    }
    let mut workspace_given = false;
    if let Ok(Query(query)) = Query::<NamedQuery>::try_from_uri(&parts.uri) {
        if let Some(workspace) = query.workspace {
            workspace_given = true;
            push(&mut named, &workspace, Named::Workspace);
        }
        if let Some(run) = query.run_id {
            push(&mut named, &run, Named::Run);
        }
        if let Some(repo) = query.repo_id {
            push(&mut named, &repo, Named::Repo);
        }
    }
    let scan_body = matches!(parts.method, Method::POST | Method::PUT | Method::PATCH)
        && path != UPLOAD_PATH
        && parts
            .headers
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| v.starts_with("application/json"));
    let body = match scan_body {
        false => body,
        true => {
            let bytes = axum::body::to_bytes(body, MAX_SCANNED_BODY)
                .await
                .map_err(|_| {
                    ApiError::new(
                        StatusCode::PAYLOAD_TOO_LARGE,
                        "too_large",
                        "The request body is too large.",
                    )
                })?;
            if let Ok(json) = serde_json::from_slice::<Value>(&bytes) {
                workspace_given |= json.get("workspace_id").is_some_and(|b| !b.is_null());
                from_json(&json, &mut named, 0);
            }
            axum::body::Body::from(bytes)
        }
    };
    let defaults = DEFAULTS_TO_MAIN
        .iter()
        .any(|(method, p)| *method == parts.method && *p == path);
    if defaults && !workspace_given {
        named.push(Named::Workspace(WorkspaceId::MAIN));
    }
    let Some(caller) = parts.extensions.get::<Caller>() else {
        return Err(ApiError::unauthorized());
    };
    let mut scope = NamedWorkspaces::default();
    for thing in &named {
        let reach = match api.engine.reach(thing.on_workspace()).await {
            Ok(reach) => reach,
            Err(Error::NotFound(_)) => return Err(hidden()),
            Err(e) => return Err(e.into()),
        };
        if !reach.visible(|b| caller.sees(b)) {
            return Err(hidden());
        }
        match reach {
            Reach::Any(workspaces) => {
                scope
                    .0
                    .push(workspaces.into_iter().filter(|b| caller.sees(*b)).collect());
            }
            Reach::All(workspaces) => scope.0.extend(workspaces.into_iter().map(|b| vec![b])),
        }
    }
    parts.extensions.insert(scope);
    Ok(next.run(Request::from_parts(parts, body)).await)
}

/// The workspace a list names with `?workspace=`; `None` (every workspace)
/// only for a deployment owner who names none. Anyone else must name one,
/// which the guard already checked they are a member of.
pub(crate) fn listed_workspace(
    caller: &Caller,
    workspace: Option<&str>,
) -> ApiResult<Option<WorkspaceId>> {
    match workspace {
        Some(text) => text
            .parse()
            .map(Some)
            .map_err(|_| ApiError::invalid("`workspace` is not a workspace id.")),
        None if caller.is_owner => Ok(None),
        None => Err(ApiError::invalid("Pass ?workspace=<workspace id>.")),
    }
}

/// The workspace `?workspace=` names, Main when a deployment owner names
/// none (see [`listed_workspace`]).
pub(crate) fn named_workspace(caller: &Caller, workspace: Option<&str>) -> ApiResult<WorkspaceId> {
    Ok(listed_workspace(caller, workspace)?.unwrap_or(WorkspaceId::MAIN))
}

/// `?workspace=` of the per-workspace lists and their items.
#[derive(serde::Deserialize)]
pub(crate) struct InWorkspace {
    pub(crate) workspace: Option<String>,
}

/// True when the caller sees where `thing` lives; true when it doesn't
/// exist (the handler answers that 404).
pub(crate) async fn visible(api: &Api, caller: &Caller, thing: OnWorkspace<'_>) -> ApiResult<bool> {
    if caller.workspaces.is_none() {
        return Ok(true);
    }
    match api.engine.reach(thing).await {
        Ok(reach) => Ok(reach.visible(|b| caller.sees(b))),
        Err(Error::NotFound(_)) => Ok(true),
        Err(e) => Err(e.into()),
    }
}

fn push<T: std::str::FromStr>(named: &mut Vec<Named>, text: &str, make: fn(T) -> Named) {
    if let Ok(id) = text.parse() {
        named.push(make(id));
    }
}

/// `path` is the matched route, e.g. `/tasks/{id}/pins/{pin}`.
fn from_path(path: &str, key: &str, value: &str, named: &mut Vec<Named>) {
    match key {
        "repo_id" => push(named, value, Named::Repo),
        "connection_id" => push(named, value, Named::VcsConnection),
        "id" if path.starts_with("/repos/") => push(named, value, Named::Repo),
        "id" if path.starts_with("/vcs/connections/") => push(named, value, Named::VcsConnection),
        "source_id" => named.push(Named::Source(value.to_owned())),
        "id" if path.starts_with("/workspaces/") || path.starts_with("/subscriptions/") => {
            push(named, value, Named::Workspace);
        }
        "id" if path.starts_with("/tasks/") => push(named, value, Named::Task),
        "id" if path.starts_with("/runs/") => push(named, value, Named::Run),
        "id" if path.starts_with("/workflows/") => push(named, value, Named::Workflow),
        "id" if path.starts_with("/knowledge/sources/") => {
            named.push(Named::Source(value.to_owned()));
        }
        "id" if path.starts_with("/knowledge/docs/") => named.push(Named::Doc(value.to_owned())),
        "id" if path.starts_with("/artifacts/") => push(named, value, Named::Artifact),
        "id" if path.starts_with("/evals/") => push(named, value, Named::Eval),
        "id" if path.starts_with("/outbox/") => push(named, value, Named::Outbox),
        "id" if path.starts_with("/projects/") => push(named, value, Named::Project),
        "id" if path.starts_with("/agents/") => push(named, value, Named::Agent),
        "id" if path.starts_with("/memory/") => push(named, value, Named::MemoryItem),
        "id" if path.starts_with("/corrections/") => push(named, value, Named::Correction),
        "id" if path.starts_with("/notes/") => push(named, value, Named::Note),
        "id" if path.starts_with("/secrets/workspace/") => push(named, value, Named::Workspace),
        _ => {}
    }
}

/// Collects the ids a JSON body names, at any depth (bounded).
fn from_json(json: &Value, named: &mut Vec<Named>, depth: u32) {
    if depth > 8 {
        return;
    }
    let one_or_many = |value: &Value| -> Vec<String> {
        match value {
            Value::String(s) => vec![s.clone()],
            Value::Array(items) => items
                .iter()
                .filter_map(|v| v.as_str().map(str::to_owned))
                .collect(),
            _ => Vec::new(),
        }
    };
    match json {
        Value::Object(map) => {
            for (key, value) in map {
                let ids = one_or_many(value);
                match key.as_str() {
                    "workspace_id" => ids.iter().for_each(|i| push(named, i, Named::Workspace)),
                    "workflow_id" => ids.iter().for_each(|i| push(named, i, Named::Workflow)),
                    "task_id" | "task_ids" => ids.iter().for_each(|i| push(named, i, Named::Task)),
                    "repo_id" | "repo_ids" => ids.iter().for_each(|i| push(named, i, Named::Repo)),
                    "set_id" => ids.iter().for_each(|i| push(named, i, Named::EvalSet)),
                    "project_id" => ids.iter().for_each(|i| push(named, i, Named::Project)),
                    // A task's `{kind: "workflow" | "agent", id}`.
                    "assignee" if value["kind"] == "workflow" => {
                        let id = value["id"].as_str().unwrap_or_default();
                        push(named, id, Named::Workflow);
                    }
                    "assignee" if value["kind"] == "agent" => {
                        let id = value["id"].as_str().unwrap_or_default();
                        push(named, id, Named::Agent);
                    }
                    "pick" => {
                        if let Ok(pick) = serde_json::from_value(value.clone()) {
                            named.push(Named::Pick(pick));
                        }
                    }
                    _ => {}
                }
                from_json(value, named, depth + 1);
            }
        }
        Value::Array(items) => items.iter().for_each(|v| from_json(v, named, depth + 1)),
        _ => {}
    }
}
