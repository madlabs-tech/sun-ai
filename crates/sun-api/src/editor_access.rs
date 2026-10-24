//! Editor access: who may read, write and open a terminal on a workspace's
//! repos in the editor ([`sun_core::editor_grant`]), and the routes that
//! grant it.
//!
//! Every editor and terminal handler calls [`require`] first, after
//! [`crate::workspace_access::guard`] (a workspace the caller can't see is
//! already 404). It resolves the caller's capabilities on every call (their
//! role is read per request, the grants in one query), never caches them, so
//! a revoke applies to the next request: a repo of another workspace is 404,
//! a missing capability 403.
//!
//! - `GET /workspaces/{id}/editor/grants`, `POST` it and `DELETE
//!   /workspaces/{id}/editor/grants/{grant}`: `edit_members` there (owners
//!   and admins), gated in `routes.rs`.
//! - `GET /workspaces/{id}/repos/{repo_id}/editor/me`: the caller's own
//!   capabilities there, for the UI to hide what they can't use (any member).

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::{Extension, Json};
use serde::Deserialize;
use serde_json::{Value, json};
use sun_core::editor_grant::{EditorCapability, EditorGrant, GrantSubject, capabilities_of};
use sun_core::model::AuditAction;
use sun_core::{EditorGrantId, HostId, RepoId, WorkspaceId};

use crate::Api;
use crate::auth::Caller;
use crate::error::{ApiError, ApiResult};
use crate::routes::{Body, id};
use crate::workspace_access::hidden;

/// What `caller` holds on `repo` of `workspace`, read now. 404 when `repo`
/// isn't a repo of `workspace` or the caller isn't in the workspace.
pub(crate) async fn capabilities(
    api: &Api,
    caller: &Caller,
    workspace: WorkspaceId,
    repo: RepoId,
) -> ApiResult<Vec<EditorCapability>> {
    let role = caller.role_in(workspace);
    if !caller.is_owner && role.is_none() {
        return Err(hidden());
    }
    let grants = api.engine.editor_grants_on(workspace, repo).await?;
    Ok(capabilities_of(
        caller.is_owner,
        caller.member_id,
        role,
        repo,
        &grants,
    ))
}

/// A capability on a repo's editor (the name `crate::editor` uses).
pub(crate) use sun_core::editor_grant::EditorCapability as Capability;

/// The guard of every editor and terminal route: `Ok` when `caller` holds
/// `cap` on `repo` of `workspace`, 403 when they don't, 404 as
/// [`capabilities`].
pub(crate) async fn require(
    api: &Api,
    caller: &Caller,
    workspace: WorkspaceId,
    repo: RepoId,
    cap: Capability,
) -> ApiResult<()> {
    held(&capabilities(api, caller, workspace, repo).await?, cap)
}

/// One audited editor action. Metadata only, never file contents or
/// keystrokes.
pub(crate) struct EditorEvent<'a> {
    pub(crate) workspace: WorkspaceId,
    pub(crate) repo: RepoId,
    pub(crate) host: HostId,
    /// `file.write`, `file.create`, `file.rename`, `file.delete`,
    /// `terminal.open` or `terminal.close`.
    pub(crate) action: &'static str,
    /// Repo-relative; a rename's is `<from> -> <to>`; empty for a
    /// terminal's open, `after <N>s` for its close.
    pub(crate) path: &'a str,
}

/// Records `event` as `caller` in the audit log, after the host confirmed
/// it: `editor_file` for files, `terminal` for terminals.
pub(crate) async fn record(api: &Api, caller: &Caller, event: EditorEvent<'_>) -> ApiResult<()> {
    let action = match event.action.starts_with("terminal.") {
        true => AuditAction::Terminal,
        false => AuditAction::EditorFile,
    };
    let detail = format!("{} {}", event.action, event.path);
    api.engine
        .audit_editor(
            &caller.actor(),
            event.workspace,
            event.repo,
            event.host,
            action,
            detail.trim_end(),
        )
        .await?;
    Ok(())
}

/// 403 unless `capability` is in `held`.
fn held(held: &[EditorCapability], capability: EditorCapability) -> ApiResult<()> {
    if held.contains(&capability) {
        return Ok(());
    }
    Err(ApiError::new(
        StatusCode::FORBIDDEN,
        "forbidden",
        format!(
            "You don't have `{capability}` on this repository. Ask a workspace owner or admin \
             to grant it."
        ),
    ))
}

/// `GET /api/v1/workspaces/{id}/repos/{repo_id}/editor/me`:
/// `{capabilities: [..]}`.
pub(crate) async fn mine(
    State(api): State<Api>,
    Extension(caller): Extension<Caller>,
    Path((workspace, repo)): Path<(String, String)>,
) -> ApiResult<Json<Value>> {
    let workspace = id(&workspace, "workspace")?;
    let repo = id(&repo, "repo")?;
    let held = capabilities(&api, &caller, workspace, repo).await?;
    Ok(Json(json!({ "capabilities": held })))
}

/// `GET /api/v1/workspaces/{id}/editor/grants`: oldest first.
pub(crate) async fn list(
    State(api): State<Api>,
    Path(workspace): Path<String>,
) -> ApiResult<Json<Vec<EditorGrant>>> {
    let workspace = id(&workspace, "workspace")?;
    Ok(Json(api.engine.editor_grants(workspace).await?))
}

/// The body of `POST /workspaces/{id}/editor/grants`.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct NewGrant {
    /// The one repo it applies to; none: every repo of the workspace.
    repo_id: Option<String>,
    /// `all`, `role` or `member`.
    subject_type: String,
    /// The role key or member id; none for `all`.
    subject_id: Option<String>,
    /// `editor.read`, `editor.write` or `terminal`.
    capability: String,
}

/// `POST /api/v1/workspaces/{id}/editor/grants {repo_id?, subject_type,
/// subject_id?, capability}`: 201 with the grant, audited. 422 for a viewer
/// given more than `editor.read`, a member not in the workspace or a grant
/// that exists; 404 for a repo of another workspace.
pub(crate) async fn create(
    State(api): State<Api>,
    Extension(caller): Extension<Caller>,
    Path(workspace): Path<String>,
    Body(input): Body<NewGrant>,
) -> ApiResult<(StatusCode, Json<EditorGrant>)> {
    let workspace = id(&workspace, "workspace")?;
    let repo = input
        .repo_id
        .as_deref()
        .map(|r| id(r, "repo"))
        .transpose()?;
    let subject = GrantSubject::from_columns(&input.subject_type, input.subject_id.as_deref())
        .map_err(|_| {
            ApiError::invalid(
                "Grant to `all`, a `role` (its key in `subject_id`) or a `member` (their id).",
            )
        })?;
    let capability = input.capability.parse().map_err(|_| {
        ApiError::invalid("`capability` is `editor.read`, `editor.write` or `terminal`.")
    })?;
    let grant = api
        .engine
        .grant_editor(
            workspace,
            repo,
            subject,
            capability,
            caller.member_id,
            &caller.actor(),
        )
        .await?;
    Ok((StatusCode::CREATED, Json(grant)))
}

/// `DELETE /api/v1/workspaces/{id}/editor/grants/{grant}`: 204, audited;
/// it applies to the holder's next request.
pub(crate) async fn revoke(
    State(api): State<Api>,
    Extension(caller): Extension<Caller>,
    Path((workspace, grant)): Path<(String, String)>,
) -> ApiResult<StatusCode> {
    let workspace = id(&workspace, "workspace")?;
    let grant: EditorGrantId = id(&grant, "grant")?;
    api.engine
        .revoke_editor(workspace, grant, &caller.actor())
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

#[cfg(test)]
mod tests {
    use axum::response::IntoResponse;

    use super::*;

    #[test]
    fn a_capability_not_held_is_403() {
        use EditorCapability::{Read, Terminal, Write};
        assert!(held(&[Read, Write], Write).is_ok());
        for (have, want) in [
            (&[][..], Read),
            (&[Read, Write][..], Terminal),
            (&[Read][..], Write),
        ] {
            let refused = held(have, want).unwrap_err().into_response();
            assert_eq!(refused.status(), StatusCode::FORBIDDEN, "{want}");
        }
    }
}
