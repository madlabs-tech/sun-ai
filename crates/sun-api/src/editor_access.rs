//! Who may use a repo's editor and terminal, and the audit of what they
//! did there. The one seam every editor and terminal handler goes through
//! (`crate::editor`).
//!
//! ponytail: owner-only stub until editor grants land (BLO-187), which
//! replace these bodies and keep the signatures.

use axum::http::StatusCode;
use sun_core::model::Role;
use sun_core::{HostId, RepoId, WorkspaceId};

use crate::Api;
use crate::auth::Caller;
use crate::error::{ApiError, ApiResult};

/// A capability on a repo's editor. Wire names: `editor.read`,
/// `editor.write`, `terminal`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Capability {
    /// Browse and read files.
    Read,
    /// Create, write, rename and delete files.
    Write,
    /// A shell on the host (Stage 2).
    #[allow(dead_code)]
    Terminal,
}

/// Ok when `caller` holds `cap` on `repo` in `workspace`, else 403. The
/// workspace guard has already answered 404 to non-members. Today: the
/// deployment owner or the workspace's owner role, for every capability.
pub(crate) async fn require(
    api: &Api,
    caller: &Caller,
    workspace: WorkspaceId,
    repo: RepoId,
    cap: Capability,
) -> ApiResult<()> {
    let _ = (api, repo, cap);
    if caller.is_owner || caller.role_in(workspace) == Some(Role::Owner) {
        return Ok(());
    }
    Err(ApiError::new(
        StatusCode::FORBIDDEN,
        "forbidden",
        "You have no editor access to this repository.",
    ))
}

/// One audited editor action. Metadata only, never file contents.
pub(crate) struct EditorEvent<'a> {
    pub(crate) workspace: WorkspaceId,
    pub(crate) repo: RepoId,
    pub(crate) host: HostId,
    /// `file.write`, `file.create`, `file.rename` or `file.delete`.
    pub(crate) action: &'static str,
    /// Repo-relative; a rename's is `<from> -> <to>`.
    pub(crate) path: &'a str,
}

/// Records `event` as `caller`, after the host confirmed it. Today: nothing
/// (the audit table comes with the grants).
pub(crate) async fn record(api: &Api, caller: &Caller, event: EditorEvent<'_>) -> ApiResult<()> {
    let _ = (api, caller, event.workspace, event.repo, event.host);
    let _ = (event.action, event.path);
    Ok(())
}
