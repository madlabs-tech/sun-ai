//! A repo's editor: REST over the file operations in the caller's editor
//! worktree on a host (`editor.open`, `fs.*` in `sun_core::host_protocol`;
//! the host confines every path to the worktree).
//!
//! Under `/workspaces/{id}/repos/{repo_id}/editor`, every call with
//! `?host_id=` (a host attached to the workspace) and, for one entry,
//! `?path=` (repo-relative, `/`-separated; `""` is the root):
//! - `POST /open`: makes the worktree on first use → `{root}`;
//! - `GET /tree`: one directory level → `{entries, truncated}`;
//! - `GET /index`: every file git doesn't ignore → `{paths, truncated}`;
//! - `GET /file`: `{size, mtime_ms, hash, too_large, binary, content}`;
//! - `PUT /file`: the raw UTF-8 body (≤ 2 MiB, not JSON) replaces the file;
//!   `If-Match: <hash>` makes it conditional → `{size, mtime_ms, hash}`, or
//!   409 `conflict` with the file's `current` version (null: deleted);
//! - `POST /file` `{kind: "file" | "dir"}`: creates it → 201;
//! - `DELETE /file` → 204; `POST /rename` `{from, to}` → 204.
//!
//! Every call passes [`editor_access::require`] first (one call site:
//! [`target`]); writes are recorded with [`editor_access::record`].

use std::time::Duration;

use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, Path, Query, State};
use axum::handler::Handler;
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Extension, Json, Router};
use serde::Deserialize;
use serde_json::{Value, json};
use sun_core::host_protocol::{EditorTree, FsKind, KernelFrame, MAX_EDITOR_FILE, ReplyBody};
use sun_core::model::Repo;
use sun_core::{Error, HostId, RepoId, WorkspaceId};
use sun_engine::WorkspaceItem;

use crate::Api;
use crate::auth::Caller;
use crate::editor_access::{self, Capability, EditorEvent};
use crate::error::{ApiError, ApiResult};
use crate::hosts::{PREPARE, REPLY, unexpected};
use crate::routes::{Body, id};

const BASE: &str = "/workspaces/{id}/repos/{repo_id}/editor";

/// The editor routes; the workspace guard runs before them.
pub(crate) fn routes() -> Router<Api> {
    let write_limit = DefaultBodyLimit::max(MAX_EDITOR_FILE as usize);
    Router::new()
        .route(&format!("{BASE}/open"), post(open))
        .route(&format!("{BASE}/tree"), get(list))
        .route(&format!("{BASE}/index"), get(index))
        .route(
            &format!("{BASE}/file"),
            get(read)
                .put(write.layer(write_limit))
                .post(create)
                .delete(delete),
        )
        .route(&format!("{BASE}/rename"), post(rename))
}

/// `?host_id=&path=`.
#[derive(Deserialize)]
pub(crate) struct At {
    host_id: String,
    #[serde(default)]
    path: String,
}

/// `POST /file` body.
#[derive(Deserialize)]
pub(crate) struct Create {
    kind: FsKind,
}

/// `POST /rename` body.
#[derive(Deserialize)]
pub(crate) struct Rename {
    from: String,
    to: String,
}

/// What a call acts on, once the caller may.
struct Target {
    workspace: WorkspaceId,
    repo: Repo,
    host: HostId,
    tree: EditorTree,
}

/// Checks the caller holds `cap`, then finds the repo on the workspace, the
/// host attached to it and the caller's worktree; 404 for either missing.
async fn target(
    api: &Api,
    caller: &Caller,
    (workspace, repo): &(String, String),
    host: &str,
    cap: Capability,
) -> ApiResult<Target> {
    let workspace: WorkspaceId = id(workspace, "workspace")?;
    let repo_id: RepoId = id(repo, "repo")?;
    let host: HostId = id(host, "host")?;
    editor_access::require(api, caller, workspace, repo_id, cap).await?;
    let member_id = caller.member_id.ok_or_else(|| {
        ApiError::new(
            StatusCode::FORBIDDEN,
            "forbidden",
            "The editor needs a signed-in member, not the deployment token.",
        )
    })?;
    let repo = api
        .engine
        .repos(workspace)
        .await?
        .into_iter()
        .find(|r| r.id == repo_id)
        .ok_or_else(|| Error::NotFound(format!("repo {repo_id}")))?;
    let attached = api.engine.workspaces_of(WorkspaceItem::Host(host)).await?;
    if !attached.contains(&workspace) {
        return Err(Error::NotFound(format!("host {host}")).into());
    }
    Ok(Target {
        workspace,
        tree: EditorTree { repo_id, member_id },
        repo,
        host,
    })
}

/// Sends one editor frame to the target's host.
async fn ask(
    api: &Api,
    target: &Target,
    wait: Duration,
    frame: impl FnOnce(u64, EditorTree) -> KernelFrame,
) -> ApiResult<ReplyBody> {
    let tree = target.tree;
    Ok(api
        .hosts
        .editor_request(target.host, wait, |id| frame(id, tree))
        .await?)
}

/// Audits a write the host confirmed.
async fn record(
    api: &Api,
    caller: &Caller,
    target: &Target,
    action: &'static str,
    path: &str,
) -> ApiResult<()> {
    let event = EditorEvent {
        workspace: target.workspace,
        repo: target.repo.id,
        host: target.host,
        action,
        path,
    };
    editor_access::record(api, caller, event).await
}

/// `POST …/editor/open`: the caller's worktree (cloned on first use).
pub(crate) async fn open(
    State(api): State<Api>,
    Extension(caller): Extension<Caller>,
    Path(ids): Path<(String, String)>,
    Query(at): Query<At>,
) -> ApiResult<Json<Value>> {
    let target = target(&api, &caller, &ids, &at.host_id, Capability::Read).await?;
    let url = target.repo.url.clone();
    let base = Some(target.repo.default_branch.clone());
    match ask(&api, &target, PREPARE, |id, tree| KernelFrame::EditorOpen {
        id,
        tree,
        url,
        base,
    })
    .await?
    {
        ReplyBody::Worktree { path } => Ok(Json(json!({ "root": path }))),
        other => Err(unexpected(other).into()),
    }
}

/// `GET …/editor/tree?path=`: one directory level.
pub(crate) async fn list(
    State(api): State<Api>,
    Extension(caller): Extension<Caller>,
    Path(ids): Path<(String, String)>,
    Query(at): Query<At>,
) -> ApiResult<Json<Value>> {
    let target = target(&api, &caller, &ids, &at.host_id, Capability::Read).await?;
    let path = at.path;
    match ask(&api, &target, REPLY, |id, tree| KernelFrame::FsList {
        id,
        tree,
        path,
    })
    .await?
    {
        ReplyBody::Entries { entries, truncated } => {
            Ok(Json(json!({ "entries": entries, "truncated": truncated })))
        }
        other => Err(unexpected(other).into()),
    }
}

/// `GET …/editor/index`: file paths for quick open.
pub(crate) async fn index(
    State(api): State<Api>,
    Extension(caller): Extension<Caller>,
    Path(ids): Path<(String, String)>,
    Query(at): Query<At>,
) -> ApiResult<Json<Value>> {
    let target = target(&api, &caller, &ids, &at.host_id, Capability::Read).await?;
    match ask(&api, &target, REPLY, |id, tree| KernelFrame::FsIndex {
        id,
        tree,
    })
    .await?
    {
        ReplyBody::Index { paths, truncated } => {
            Ok(Json(json!({ "paths": paths, "truncated": truncated })))
        }
        other => Err(unexpected(other).into()),
    }
}

/// `GET …/editor/file?path=`.
pub(crate) async fn read(
    State(api): State<Api>,
    Extension(caller): Extension<Caller>,
    Path(ids): Path<(String, String)>,
    Query(at): Query<At>,
) -> ApiResult<Json<Value>> {
    let target = target(&api, &caller, &ids, &at.host_id, Capability::Read).await?;
    let path = at.path;
    match ask(&api, &target, REPLY, |id, tree| KernelFrame::FsRead {
        id,
        tree,
        path,
    })
    .await?
    {
        ReplyBody::File { file } => Ok(Json(json!(file))),
        other => Err(unexpected(other).into()),
    }
}

/// `PUT …/editor/file?path=` with the raw text, `If-Match: <hash>` optional.
pub(crate) async fn write(
    State(api): State<Api>,
    Extension(caller): Extension<Caller>,
    Path(ids): Path<(String, String)>,
    Query(at): Query<At>,
    headers: HeaderMap,
    body: Bytes,
) -> ApiResult<Response> {
    let target = target(&api, &caller, &ids, &at.host_id, Capability::Write).await?;
    let content = String::from_utf8(body.to_vec())
        .map_err(|_| ApiError::invalid("The editor saves UTF-8 text only."))?;
    let expected_hash = match headers.get(header::IF_MATCH) {
        None => None,
        Some(value) => Some(
            value
                .to_str()
                .map_err(|_| ApiError::invalid("`If-Match` is not a file hash."))?
                .trim()
                .trim_matches('"')
                .to_owned(),
        ),
    };
    let path = at.path.clone();
    let reply = ask(&api, &target, REPLY, |id, tree| KernelFrame::FsWrite {
        id,
        tree,
        path,
        content,
        expected_hash,
    })
    .await?;
    match reply {
        ReplyBody::Stat { stat } => {
            record(&api, &caller, &target, "file.write", &at.path).await?;
            Ok(Json(json!(stat)).into_response())
        }
        ReplyBody::Conflict { current } => {
            let body = json!({
                "error": {
                    "code": "conflict",
                    "message": "The file changed since you opened it.",
                },
                "current": current,
            });
            Ok((StatusCode::CONFLICT, Json(body)).into_response())
        }
        other => Err(unexpected(other).into()),
    }
}

/// `POST …/editor/file?path=` `{kind}`: a new empty file or directory.
pub(crate) async fn create(
    State(api): State<Api>,
    Extension(caller): Extension<Caller>,
    Path(ids): Path<(String, String)>,
    Query(at): Query<At>,
    Body(create): Body<Create>,
) -> ApiResult<StatusCode> {
    let target = target(&api, &caller, &ids, &at.host_id, Capability::Write).await?;
    let (path, kind) = (at.path.clone(), create.kind);
    let reply = ask(&api, &target, REPLY, |id, tree| KernelFrame::FsCreate {
        id,
        tree,
        path,
        kind,
    })
    .await?;
    done(reply)?;
    record(&api, &caller, &target, "file.create", &at.path).await?;
    Ok(StatusCode::CREATED)
}

/// `DELETE …/editor/file?path=`: a file, symlink or whole directory.
pub(crate) async fn delete(
    State(api): State<Api>,
    Extension(caller): Extension<Caller>,
    Path(ids): Path<(String, String)>,
    Query(at): Query<At>,
) -> ApiResult<StatusCode> {
    let target = target(&api, &caller, &ids, &at.host_id, Capability::Write).await?;
    let path = at.path.clone();
    let reply = ask(&api, &target, REPLY, |id, tree| KernelFrame::FsDelete {
        id,
        tree,
        path,
    })
    .await?;
    done(reply)?;
    record(&api, &caller, &target, "file.delete", &at.path).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// `POST …/editor/rename` `{from, to}`.
pub(crate) async fn rename(
    State(api): State<Api>,
    Extension(caller): Extension<Caller>,
    Path(ids): Path<(String, String)>,
    Query(at): Query<At>,
    Body(rename): Body<Rename>,
) -> ApiResult<StatusCode> {
    let target = target(&api, &caller, &ids, &at.host_id, Capability::Write).await?;
    let moved = format!("{} -> {}", rename.from, rename.to);
    let Rename { from, to } = rename;
    let reply = ask(&api, &target, REPLY, |id, tree| KernelFrame::FsRename {
        id,
        tree,
        from,
        to,
    })
    .await?;
    done(reply)?;
    record(&api, &caller, &target, "file.rename", &moved).await?;
    Ok(StatusCode::NO_CONTENT)
}

fn done(reply: ReplyBody) -> ApiResult<()> {
    match reply {
        ReplyBody::Done => Ok(()),
        other => Err(unexpected(other).into()),
    }
}
