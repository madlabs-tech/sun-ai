//! Invitations into a workspace with a role. An admin there invites an
//! email (`invite_members`, never above their own role); the person gets a
//! link `{public_url}/#/invite/<token>` by mail, or, when the deployment
//! can't send mail, the admin gets it in the answer to pass on. The token is
//! 32 random bytes (base64url), stored as `sha256(token)` only; it works for
//! 7 days, once. The link's page reads the invitation without signing in
//! ([`lookup`]); the person signs up or in with the invited email and
//! accepts or declines it ([`accept`], [`decline`]). Accepting never lowers
//! a role they already have there.

use std::time::Duration;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::{Extension, Json};
use base64::Engine as _;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sun_core::entitlement::Quota;
use sun_core::model::{Mail, Permission, Role, WorkspaceInvitation, WorkspaceMember};
use sun_core::{Error, InvitationId, MemberId, Timestamp, WorkspaceId};

use super::members::may_grant;
use super::users::send;
use super::{Caller, digest};
use crate::Api;
use crate::entitlement::within_plan;
use crate::error::{ApiError, ApiResult};
use crate::routes::{Body, id};
use crate::workspace_access::hidden;

/// How long an invitation link works.
const INVITATION_TTL: Duration = Duration::from_secs(7 * 24 * 60 * 60);
/// A link token: 32 bytes, base64url without padding.
const TOKEN_LEN: usize = 43;

/// 32 random bytes from the OS RNG, base64url.
fn new_token() -> ApiResult<String> {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).map_err(|e| Error::Unavailable(format!("no randomness: {e}")))?;
    Ok(base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct NewInvitation {
    email: String,
    role: Role,
}

/// `POST /api/v1/workspaces/{id}/invitations {email, role}`
/// (`invite_members` there, `role` at most the caller's own): replaces a
/// pending invitation to the same email; 422 when they already are a
/// member, 403 for a plan without a free seat. 201 `{invitation, mailed,
/// link}`: `link` is set only when the deployment can't send mail
/// (`mailed: false`), for the admin to pass on.
pub(crate) async fn create(
    State(api): State<Api>,
    Extension(caller): Extension<Caller>,
    Path(workspace): Path<String>,
    Body(input): Body<NewInvitation>,
) -> ApiResult<(StatusCode, Json<Value>)> {
    let workspace: WorkspaceId = id(&workspace, "workspace")?;
    let email = sun_core::model::normalize_email(&input.email)?;
    may_grant(&caller, workspace, input.role, Permission::InviteMembers)?;
    let seats = api.engine.workspace_members(workspace).await?.len();
    within_plan(&api, workspace, Quota::Seats, seats).await?;
    let token = new_token()?;
    let expires_at = chrono::Utc::now() + INVITATION_TTL;
    let invitation = api
        .engine
        .invite(
            workspace,
            &email,
            input.role,
            caller.member_id,
            &digest(&token),
            expires_at,
            &caller.actor(),
        )
        .await?;
    let link = format!(
        "{}/#/invite/{token}",
        api.config.public_url.trim_end_matches('/')
    );
    let can_mail = api.mailer.is_some() && api.engine.smtp_settings().await?.0.is_some();
    if !can_mail {
        return Ok((
            StatusCode::CREATED,
            Json(json!({ "invitation": invitation, "mailed": false, "link": link })),
        ));
    }
    let name = api.engine.workspace(workspace).await?.name;
    let mail = Mail {
        to: email,
        subject: format!("You're invited to {name} on Sun"),
        body: format!(
            "Hi,\n\n{} invited you to the workspace {name} on Sun as {}.\n\n\
             Open this link to join (it works once, for 7 days):\n{link}\n\n\
             If you didn't expect this, ignore this mail.\n",
            caller.principal.user, input.role
        ),
    };
    send(&api, &mail).await?;
    Ok((
        StatusCode::CREATED,
        Json(json!({ "invitation": invitation, "mailed": true, "link": null })),
    ))
}

/// `GET /api/v1/workspaces/{id}/invitations` (`invite_members` there): the
/// pending ones, newest first.
pub(crate) async fn list(
    State(api): State<Api>,
    Path(workspace): Path<String>,
) -> ApiResult<Json<Vec<WorkspaceInvitation>>> {
    let workspace: WorkspaceId = id(&workspace, "workspace")?;
    api.engine.workspace(workspace).await?;
    Ok(Json(api.engine.invitations(workspace).await?))
}

/// `DELETE /api/v1/workspaces/{id}/invitations/{invitation}`
/// (`invite_members` there): revokes a pending invitation of this
/// workspace (its link stops working); 404 for any other.
pub(crate) async fn revoke(
    State(api): State<Api>,
    Extension(caller): Extension<Caller>,
    Path((workspace, invitation)): Path<(String, String)>,
) -> ApiResult<StatusCode> {
    let workspace: WorkspaceId = id(&workspace, "workspace")?;
    let invitation: InvitationId = id(&invitation, "invitation")?;
    let pending = api.engine.invitations(workspace).await?;
    if !pending.iter().any(|i| i.id == invitation) {
        return Err(hidden());
    }
    api.engine
        .revoke_invitation(invitation, &caller.actor())
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

/// An invitation as its invitee sees it: where to, as what, from whom.
#[derive(Serialize)]
pub(crate) struct InvitationView {
    id: InvitationId,
    workspace_id: WorkspaceId,
    workspace_name: String,
    email: String,
    role: Role,
    /// The inviter's username (`None`: the token holder, or a removed member).
    inviter: Option<String>,
    expires_at: Timestamp,
}

async fn view(api: &Api, invitation: WorkspaceInvitation) -> ApiResult<InvitationView> {
    let inviter = match invitation.inviter_id {
        Some(inviter) => match api.engine.member(inviter).await {
            Ok(member) => Some(member.username),
            Err(Error::NotFound(_)) => None,
            Err(e) => return Err(e.into()),
        },
        None => None,
    };
    Ok(InvitationView {
        workspace_name: api.engine.workspace(invitation.workspace_id).await?.name,
        id: invitation.id,
        workspace_id: invitation.workspace_id,
        email: invitation.email,
        role: invitation.role,
        inviter,
        expires_at: invitation.expires_at,
    })
}

/// `GET /api/v1/invitations/{token}` (open): the pending invitation the
/// link's token opens; 404 when it is unknown, used, declined, revoked or
/// expired.
pub(crate) async fn lookup(
    State(api): State<Api>,
    Path(token): Path<String>,
) -> ApiResult<Json<InvitationView>> {
    let well_formed = token.len() == TOKEN_LEN
        && token
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_');
    if !well_formed {
        return Err(hidden());
    }
    let invitation = api
        .engine
        .invitation_by_token(&digest(&token))
        .await?
        .filter(|i| i.is_pending(chrono::Utc::now()))
        .ok_or_else(hidden)?;
    Ok(Json(view(&api, invitation).await?))
}

/// `GET /api/v1/me/invitations`: the pending invitations to the caller's
/// email, newest first.
pub(crate) async fn mine(
    State(api): State<Api>,
    Extension(caller): Extension<Caller>,
) -> ApiResult<Json<Vec<InvitationView>>> {
    let Some(me) = caller.member_id else {
        return Ok(Json(Vec::new()));
    };
    let mut views = Vec::new();
    for invitation in api.engine.my_invitations(me).await? {
        views.push(view(&api, invitation).await?);
    }
    Ok(Json(views))
}

/// The caller's member id, or 422 for the token holder.
fn invitee(caller: &Caller) -> ApiResult<MemberId> {
    caller.member_id.ok_or_else(|| {
        ApiError::invalid("The kernel token holder isn't invited anywhere: sign in as yourself.")
    })
}

/// A used, declined, revoked or expired invitation reads like a missing one.
fn not_pending(error: Error) -> ApiError {
    match error {
        Error::InvalidInput(_) => hidden(),
        e => e.into(),
    }
}

/// `POST /api/v1/invitations/{id}/accept`: the caller joins its workspace
/// with its role (keeping a higher one they have there). 403 unless it is
/// to their verified email, or when joining needs a seat the workspace's
/// plan doesn't have; 404 when it isn't pending.
pub(crate) async fn accept(
    State(api): State<Api>,
    Extension(caller): Extension<Caller>,
    Path(invitation): Path<String>,
) -> ApiResult<Json<WorkspaceMember>> {
    let invitation: InvitationId = id(&invitation, "invitation")?;
    let me = invitee(&caller)?;
    let mine = api.engine.my_invitations(me).await?;
    // Not theirs: the engine answers as for any other invitation.
    let workspace = mine
        .iter()
        .find(|i| i.id == invitation)
        .map_or(WorkspaceId::MAIN, |i| i.workspace_id);
    let plan = api.entitlements.for_workspace(workspace).await;
    let joined = api
        .engine
        .accept_invitation(invitation, me, &plan)
        .await
        .map_err(not_pending)?;
    Ok(Json(joined))
}

/// `POST /api/v1/invitations/{id}/decline`: as [`accept`], without joining.
pub(crate) async fn decline(
    State(api): State<Api>,
    Extension(caller): Extension<Caller>,
    Path(invitation): Path<String>,
) -> ApiResult<StatusCode> {
    let invitation: InvitationId = id(&invitation, "invitation")?;
    api.engine
        .decline_invitation(invitation, invitee(&caller)?)
        .await
        .map_err(not_pending)?;
    Ok(StatusCode::NO_CONTENT)
}
