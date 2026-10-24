//! Sign-in, sessions and workspace roles.
//!
//! - **Kernel token** (the CLI): `Authorization: Bearer <token>`. The holder
//!   is an owner acting as [`crate::ApiConfig::principal`]. Browsers never
//!   use it: their sessions are always a member's.
//! - **Email + password** ([`users`]): `POST /api/v1/session`
//!   checks the Argon2id password, then the member's emailed 6-digit code
//!   (an invite or a reset: once, within [`users::CODE_TTL`], dead after 5
//!   misses), which must then be replaced by a password
//!   ([`Caller::must_set_password`]) before anything else, as must a
//!   password an admin set with "change at next sign-in". Every failure is
//!   the same 401, and failures are rate-limited per email and client IP
//!   ([`Limiter`]). Deactivated members can't sign in. The first-run owner
//!   is `admin@sun.local` with the kernel token as the password, and must
//!   replace that email first ([`Caller::needs_email`]).
//! - **2-step** ([`two_step`]): after a right password, a member with a
//!   second step (an authenticator, emailed codes or a passkey, [`passkey`])
//!   gets a challenge, not a
//!   session; `POST /session/two_step` answers it. The `security` policy can
//!   require a second step ([`Caller::must_enroll_two_step`]) and ask for it
//!   to change a password. The first-run owner's first own password rotates
//!   the kernel token (it was printed, maybe into logs).
//! - **SSO** ([`sso`]): Google, GitHub and OIDC providers admins
//!   manage in Settings → Sign-in; authorization code + PKCE. Sessions it
//!   starts skip Sun's 2-step (the identity provider owns that).
//! - A sign-in puts a random session token in the `sun_token` cookie
//!   (`EventSource` cannot send headers); the session maps it to a
//!   [`Member`], whose workspace roles are read on every request so a change applies
//!   at once. Sessions are stored (as `sha256(token)` only), so a restart
//!   signs nobody out; they last [`SESSION_TTL`]. Sign-out deletes the row.
//! - Every authenticated request carries a [`Caller`] with their role in
//!   each workspace (a deployment owner may do everything), read per
//!   request. [`crate::workspace_access::guard`] stores the workspaces a
//!   request names ([`NamedWorkspaces`]); [`require_by_method`] lets anyone
//!   read (`GET`) and writing needs `run` there; [`require`] guards routes
//!   by permission in each named workspace (a route naming none is for
//!   deployment owners); [`require_owner`] guards the deployment's own
//!   settings and accounts. Approvals pass the caller's [`Principal`] in
//!   the task's workspace (username and role keys) to the engine.

mod invitations;
mod members;
mod passkey;
mod sso;
mod two_step;
mod users;

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use axum::extract::{ConnectInfo, Request, State};
use axum::http::header::{AUTHORIZATION, COOKIE, SET_COOKIE};
use axum::http::{HeaderMap, Method, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::{Extension, Json};
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use sun_core::model::{FIRST_ADMIN_EMAIL, Permission, Role, WorkspaceMember, roles_of};
use sun_core::{Error, MemberId, WorkspaceId};
use sun_engine::{Access, Principal};

use crate::Api;
use crate::error::{ApiError, ApiResult};
use crate::hosts::random_hex;
use crate::workspace_access::NamedWorkspaces;

pub(crate) use invitations::{
    accept as accept_invitation, create as create_invitation, decline as decline_invitation,
    list as list_invitations, lookup as look_up_invitation, mine as my_invitations,
    revoke as revoke_invitation,
};
pub(crate) use members::{
    audit_log, leave_workspace, list_members, remove_workspace_member, set_owner,
    set_workspace_role, workspace_members,
};
pub(crate) use passkey::{confirm as confirm_my_passkey, start as start_my_passkey};
pub use sso::PresetUrls;
pub(crate) use sso::{
    SsoState, callback as sso_callback, delete as delete_sso, list as list_sso, login as sso_login,
    providers, put as put_sso, redirect, secure,
};
pub(crate) use two_step::{
    add_email as add_my_two_step_email, confirm_totp as confirm_my_totp,
    get_policy as get_security_policy, mail_code as mail_two_step_code, mine as my_two_step,
    my_challenge as my_two_step_challenge, put_policy as put_security_policy,
    remove as remove_my_two_step, reset as reset_member_two_step, sign_in as sign_in_two_step,
    start_totp as start_my_totp,
};
pub(crate) use users::{
    activate_member, deactivate_member, get_smtp, invite_member, put_smtp, reinvite_member,
    set_member_password, set_my_email, set_my_password, signup, test_smtp,
};

const COOKIE_NAME: &str = "sun_token";
/// How long a session lasts before the person signs in again.
const SESSION_TTL: Duration = Duration::from_secs(12 * 60 * 60);

/// Who is calling, resolved by [`authenticate`].
#[derive(Debug, Clone)]
pub(crate) struct Caller {
    /// Who they are for approvals (`who: user:<name>`); [`Caller::principal_in`]
    /// adds their role keys in a workspace.
    pub(crate) principal: Principal,
    /// Deployment owners may do everything, everywhere.
    pub(crate) is_owner: bool,
    /// The member's workspaces and role in each (none for the token holder).
    pub(crate) memberships: Vec<WorkspaceMember>,
    /// The workspaces they see (their memberships); `None` for every
    /// workspace (deployment owners). Enforced by [`crate::workspace_access::guard`].
    pub(crate) workspaces: Option<Vec<WorkspaceId>>,
    /// How the session started: `token` (the CLI), `sso` (an SSO sign-in,
    /// any provider) or `local` (a password or code).
    via: &'static str,
    email: Option<String>,
    /// They proved their email (a code or SSO); the token holder always.
    email_verified: bool,
    /// The member row, `None` for the token holder.
    pub(crate) member_id: Option<MemberId>,
    /// Signed in with an invite code: only `PUT /me/password` until done.
    must_set_password: bool,
    /// A member from before invites without an email, or the first-run owner's
    /// placeholder email: only `PUT /me/email`.
    needs_email: bool,
    /// The policy requires a second step they don't have: only adding one.
    must_enroll_two_step: bool,
}

impl Caller {
    /// Who a task's activity names: the member, and the audit actor.
    pub(crate) fn task_actor(&self) -> sun_engine::Actor {
        sun_engine::Actor {
            member_id: self.member_id,
            name: self.actor(),
        }
    }

    /// The audit actor, e.g. `user:alice`.
    pub(crate) fn actor(&self) -> String {
        format!("user:{}", self.principal.user)
    }

    /// The caller's email, when the sign-in carried one (OIDC).
    pub(crate) fn email(&self) -> Option<&str> {
        self.email.as_deref()
    }

    /// True when the caller sees `workspace`.
    pub(crate) fn sees(&self, workspace: WorkspaceId) -> bool {
        self.workspaces
            .as_ref()
            .is_none_or(|b| b.contains(&workspace))
    }

    /// [`Self::sees`] for a workspace id as text (wire views); false if malformed.
    pub(crate) fn sees_text(&self, workspace: &str) -> bool {
        self.workspaces.is_none() || workspace.parse().is_ok_and(|b| self.sees(b))
    }

    /// Their role in `workspace`, if they are a member.
    pub(crate) fn role_in(&self, workspace: WorkspaceId) -> Option<Role> {
        self.memberships
            .iter()
            .find(|m| m.workspace_id == workspace)
            .map(|m| m.role)
    }

    /// True when they may `permission` in `workspace`: a deployment owner
    /// always, anyone else when their role there grants it.
    pub(crate) fn can_in(&self, workspace: WorkspaceId, permission: Permission) -> bool {
        self.is_owner
            || self
                .role_in(workspace)
                .is_some_and(|r| r.permissions().contains(&permission))
    }

    /// Who they are for an approval on a task in `workspace`: their
    /// username and the keys of their role there and those below it (every
    /// key for a deployment owner; the token holder's configured principal).
    pub(crate) fn principal_in(&self, workspace: WorkspaceId) -> Principal {
        if self.member_id.is_none() {
            return self.principal.clone();
        }
        let roles: Vec<Role> = self.role_in(workspace).into_iter().collect();
        Principal {
            user: self.principal.user.clone(),
            roles: roles_of(self.is_owner, &roles),
        }
    }

    /// A signed-in member; `sso` when an SSO sign-in started the session.
    fn member(access: Access, sso: bool) -> Self {
        let Access {
            member,
            memberships,
            workspaces,
            ..
        } = access;
        let via = if sso { "sso" } else { "local" };
        Self {
            principal: Principal {
                user: member.username.clone(),
                roles: roles_of(member.is_owner, &[]),
            },
            workspaces,
            is_owner: member.is_owner,
            memberships,
            via,
            needs_email: needs_email(member.email.as_deref()),
            email_verified: member.email_verified_at.is_some(),
            email: member.email,
            member_id: Some(member.id),
            must_set_password: member.must_set_password,
            must_enroll_two_step: false,
        }
    }
}

/// The 403 for a role without `permission`.
pub(crate) fn denied(permission: Permission) -> ApiError {
    let what = match permission {
        Permission::Run => "run tasks",
        Permission::Approve => "approve",
        Permission::ManagePlugins => "manage plugins",
        Permission::ManageExtensions => "manage extensions",
        Permission::ManageSecrets => "manage secrets",
        Permission::ManageRuntimes => "manage runtimes",
        Permission::ManageSettings => "manage settings",
        Permission::InviteMembers => "invite members",
        Permission::CreateMembers => "add members with a password",
        Permission::ViewMembers => "see members",
        Permission::EditMembers => "edit members",
        Permission::DeactivateMembers => "deactivate members",
        Permission::EditWorkspace => "edit workspaces",
        Permission::DeleteWorkspace => "delete workspaces",
        Permission::CreateWorkflow => "create workflows",
        Permission::EditWorkflow => "edit workflows",
    };
    forbidden(format!(
        "Your role in this workspace can't {what}. Ask a workspace admin for a role with the \
         `{permission}` permission."
    ))
}

/// Sign-in state shared by every handler: pending SSO logins and the
/// sign-in limiters. Sessions live in the store.
#[derive(Default)]
pub(crate) struct AuthState {
    pub(crate) sso: SsoState,
    /// Sign-in tries per `(email, client IP)`.
    limiter: Limiter,
    /// `PUT /me/password` tries per `(member id, client IP)`.
    current_password: Limiter,
    /// `POST /signup` tries per email.
    signup: Limiter,
}

/// Tries per `(key, client IP)` within [`LIMIT_WINDOW`]. Each try reserves
/// its slot before the slow check, so parallel tries can't all pass; a
/// right one clears the key.
// ponytail: in memory and per process, pruned on every try; a shared
// store (or a proxy's limit) if Sun ever runs as several kernels.
#[derive(Default)]
pub(crate) struct Limiter(Mutex<HashMap<(String, String), (Instant, u32)>>);

/// Failed tries allowed per key and IP within [`LIMIT_WINDOW`].
const MAX_FAILURES: u32 = 10;
const LIMIT_WINDOW: Duration = Duration::from_secs(15 * 60);

impl Limiter {
    fn key(key: &str, ip: &str) -> (String, String) {
        (key.trim().to_ascii_lowercase(), ip.to_owned())
    }

    /// Counts a try for this key and IP, or answers 429 when they used up
    /// their tries. Check and count happen under one lock.
    fn reserve(&self, key: &str, ip: &str) -> ApiResult<()> {
        let mut map = self.0.lock().unwrap_or_else(|e| e.into_inner());
        map.retain(|_, (start, _)| start.elapsed() < LIMIT_WINDOW);
        let entry = map.entry(Self::key(key, ip)).or_insert((Instant::now(), 0));
        if entry.1 >= MAX_FAILURES {
            return Err(ApiError::new(
                StatusCode::TOO_MANY_REQUESTS,
                "rate_limited",
                "Too many tries. Wait 15 minutes, then try again.",
            ));
        }
        entry.1 += 1;
        Ok(())
    }

    /// Gives back the try [`Self::reserve`] counted but keeps earlier
    /// failures (a right password that still needs its second step).
    fn release(&self, key: &str, ip: &str) {
        let mut map = self.0.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(entry) = map.get_mut(&Self::key(key, ip)) {
            entry.1 = entry.1.saturating_sub(1);
        }
    }

    /// A right try: forget this key's failures.
    fn succeed(&self, key: &str, ip: &str) {
        let mut map = self.0.lock().unwrap_or_else(|e| e.into_inner());
        map.remove(&Self::key(key, ip));
    }
}

/// The client's IP, or `unknown` when the server wasn't given one.
fn client_ip(connect: Option<Extension<ConnectInfo<SocketAddr>>>) -> String {
    connect.map_or_else(|| "unknown".to_owned(), |c| c.0.0.ip().to_string())
}

/// No email yet, or still the first-run placeholder.
fn needs_email(email: Option<&str>) -> bool {
    email.is_none_or(|e| e == FIRST_ADMIN_EMAIL)
}

pub(crate) fn digest(token: &str) -> String {
    Sha256::digest(token.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// Resolves the [`Caller`] from the token or session, or answers 401.
pub(crate) async fn authenticate(
    State(api): State<Api>,
    mut request: Request,
    next: Next,
) -> Result<Response, ApiError> {
    let caller = resolve(&api, request.headers())
        .await?
        .ok_or_else(ApiError::unauthorized)?;
    gate(&caller, request.method(), request.uri().path())?;
    request.extensions_mut().insert(caller);
    Ok(next.run(request).await)
}

async fn resolve(api: &Api, headers: &HeaderMap) -> ApiResult<Option<Caller>> {
    let Some(token) = presented(headers) else {
        return Ok(None);
    };
    // The token holder installed Sun: always an owner. Their member row
    // (email) is read only where it's needed (`/me`, the SMTP test).
    let owner = Caller {
        principal: api.config.principal.clone(),
        is_owner: true,
        memberships: Vec::new(),
        workspaces: None,
        via: "token",
        email: None,
        email_verified: true,
        member_id: None,
        must_set_password: false,
        needs_email: false,
        must_enroll_two_step: false,
    };
    if same(token.as_bytes(), api.token().as_bytes()) {
        return Ok(Some(owner));
    }
    let Some((id, sso)) = api.engine.login_session(&digest(token)).await? else {
        return Ok(None);
    };
    match api.engine.member_access(id).await {
        // Deactivating ends sessions; this also covers one racing it.
        Ok(access) if access.member.deactivated_at.is_some() => Ok(None),
        Ok(access) => {
            let without = !access.member.has_two_step;
            let caller = Caller::member(access, sso);
            // Sessions an SSO sign-in started skip Sun's 2-step (the identity
            // provider owns it); every password or code session doesn't.
            let must_enroll_two_step =
                without && !sso && api.engine.security_policy().await?.require_two_step;
            let caller = Caller {
                must_enroll_two_step,
                ..caller
            };
            Ok(Some(caller))
        }
        Err(Error::NotFound(_)) => Ok(None),
        Err(e) => Err(e.into()),
    }
}

/// A member who signed in with a code (or whose admin-set password must
/// change) may only set their password, one without an email (or with the
/// first-run placeholder) only give it, and one the policy requires a second
/// step of only add one, in that order, until done (403 otherwise).
/// `path` is relative to `/api/v1`.
fn gate(caller: &Caller, method: &Method, path: &str) -> ApiResult<()> {
    let open = |allowed: &str| {
        (method == Method::GET && path == "/me") || (method == Method::PUT && path == allowed)
    };
    if caller.must_set_password && !open("/me/password") {
        return Err(ApiError::new(
            StatusCode::FORBIDDEN,
            "password_required",
            "Set a password first.",
        ));
    }
    if caller.needs_email && !caller.must_set_password && !open("/me/email") {
        return Err(ApiError::new(
            StatusCode::FORBIDDEN,
            "email_required",
            "Add your email first.",
        ));
    }
    let enrolling = (method == Method::GET && ["/me", "/me/two_step"].contains(&path))
        || (method == Method::POST
            && [
                "/me/two_step/totp",
                "/me/two_step/totp/confirm",
                "/me/two_step/email",
                "/me/two_step/passkey",
                "/me/two_step/passkey/confirm",
            ]
            .contains(&path));
    if caller.must_enroll_two_step && !caller.must_set_password && !caller.needs_email && !enrolling
    {
        return Err(ApiError::new(
            StatusCode::FORBIDDEN,
            "two_step_required",
            "Set up 2-step verification first.",
        ));
    }
    Ok(())
}

/// Anyone may read (the guard already hid other workspaces); writing needs
/// the `run` permission in every workspace the request names.
pub(crate) async fn require_by_method(request: Request, next: Next) -> ApiResult<Response> {
    if !matches!(*request.method(), Method::GET | Method::HEAD) {
        allowed(&request, &[Permission::Run])?;
    }
    Ok(next.run(request).await)
}

/// Middleware: one of `permissions` in every workspace the request names
/// ([`NamedWorkspaces`]), for every method. A request naming none is for
/// deployment owners only.
pub(crate) fn require(
    permissions: &'static [Permission],
) -> impl Fn(Request, Next) -> std::pin::Pin<Box<dyn Future<Output = ApiResult<Response>> + Send>> + Clone
{
    move |request, next| {
        Box::pin(async move {
            allowed(&request, permissions)?;
            Ok(next.run(request).await)
        })
    }
}

/// 403 unless the caller is a deployment owner or holds one of
/// `permissions` in each workspace (group) the request names.
fn allowed(request: &Request, permissions: &[Permission]) -> ApiResult<()> {
    let named = request.extensions().get::<NamedWorkspaces>();
    allowed_in(caller(request)?, named, permissions)
}

/// [`allowed`] for a handler that checks a permission only some bodies
/// need, on the workspaces the guard found ([`NamedWorkspaces`]).
pub(crate) fn allowed_in(
    caller: &Caller,
    named: Option<&NamedWorkspaces>,
    permissions: &[Permission],
) -> ApiResult<()> {
    if caller.is_owner {
        return Ok(());
    }
    let named = named.map_or(&[][..], |n| n.0.as_slice());
    if named.is_empty() {
        return Err(owner_only());
    }
    for any in named {
        let may = any
            .iter()
            .any(|w| permissions.iter().any(|p| caller.can_in(*w, *p)));
        if !may {
            return Err(denied(
                permissions.first().copied().unwrap_or(Permission::Run),
            ));
        }
    }
    Ok(())
}

fn owner_only() -> ApiError {
    forbidden("Only a deployment owner can do this.".into())
}

/// Only deployment owners: the deployment's own settings (SMTP, sign-in,
/// security, audit, hosts, mods, ecosystem secrets, runtimes, limits,
/// catalog) and every account on it.
pub(crate) async fn require_owner(request: Request, next: Next) -> ApiResult<Response> {
    if !caller(&request)?.is_owner {
        return Err(owner_only());
    }
    Ok(next.run(request).await)
}

fn caller(request: &Request) -> ApiResult<&Caller> {
    request
        .extensions()
        .get::<Caller>()
        .ok_or_else(ApiError::unauthorized)
}

fn forbidden(message: String) -> ApiError {
    ApiError::new(StatusCode::FORBIDDEN, "forbidden", message)
}

/// `POST /api/v1/session {email, password}`. Missing fields are empty, so
/// any other body (e.g. the removed `{token}`) is the same 401.
#[derive(Deserialize)]
pub(crate) struct SignIn {
    #[serde(default)]
    email: String,
    #[serde(default)]
    password: String,
}

/// The `Set-Cookie` value that starts `session`.
fn session_cookie(session: &str) -> String {
    format!(
        "{COOKIE_NAME}={session}; Path=/; HttpOnly; SameSite=Strict; Max-Age={}",
        SESSION_TTL.as_secs()
    )
}

/// The one answer to every failed email sign-in: it never says which part
/// was wrong, or whether the email is a member.
fn wrong_credentials() -> ApiError {
    ApiError::new(
        StatusCode::UNAUTHORIZED,
        "unauthorized",
        "That email, password or code is not right.",
    )
}

/// `POST /api/v1/session {email, password}`: checks the password, then
/// the member's emailed code (the same box), audits the login, and answers
/// with an `HttpOnly; SameSite=Strict` session cookie, or, when a right
/// password needs a second step, `200 {two_step:{challenge, methods}}` and
/// no cookie (answer it at `POST /session/two_step`). Every failure is the
/// same 401; failures are rate-limited per email and client IP.
pub(crate) async fn sign_in(
    State(api): State<Api>,
    connect: Option<Extension<ConnectInfo<SocketAddr>>>,
    crate::routes::Body(input): crate::routes::Body<SignIn>,
) -> Result<Response, ApiError> {
    let session = random_hex()?;
    let expires_at = chrono::Utc::now() + SESSION_TTL;
    let ip = client_ip(connect);
    let SignIn { email, password } = input;
    api.auth.limiter.reserve(&email, &ip)?;
    let signed_in = match users::password_sign_in(&api, &email, &password).await? {
        Some(member) => {
            // The password was right; earlier failures count on until the
            // second step passes too.
            if let Some(two_step) = two_step::after_password(&api, &member).await? {
                api.auth.limiter.release(&email, &ip);
                return Ok((StatusCode::OK, Json(two_step)).into_response());
            }
            api.engine
                .sign_in_password(&member, &digest(&session), expires_at)
                .await?;
            true
        }
        None => users::code_sign_in(&api, &email, &password, &digest(&session), expires_at).await?,
    };
    if !signed_in {
        return Err(wrong_credentials());
    }
    api.auth.limiter.succeed(&email, &ip);
    Ok((
        StatusCode::NO_CONTENT,
        [(SET_COOKIE, session_cookie(&session))],
    )
        .into_response())
}

/// `DELETE /api/v1/session`: ends the session (if any) and clears the cookie.
pub(crate) async fn sign_out(State(api): State<Api>, headers: HeaderMap) -> ApiResult<Response> {
    if let Some(token) = presented(&headers) {
        api.engine.end_login_session(&digest(token)).await?;
    }
    let cookie = format!("{COOKIE_NAME}=; Path=/; HttpOnly; SameSite=Strict; Max-Age=0");
    Ok((StatusCode::NO_CONTENT, [(SET_COOKIE, cookie)]).into_response())
}

/// `GET /api/v1/me`: who the caller is, their workspaces and role in each
/// (`memberships`), whether they are a deployment owner (`is_owner`) and
/// proved their email (`email_verified`), the role view they picked, and
/// what they must do first (`needs_email`, `must_set_password`,
/// `must_enroll_two_step`), and whether the bypass and terminal policies let
/// them (`can_bypass`, `can_terminal`; a terminal also needs its grant). For the token holder, `needs_email` asks
/// onboarding for the owner's email.
pub(crate) async fn me(
    State(api): State<Api>,
    Extension(caller): Extension<Caller>,
) -> ApiResult<Json<Value>> {
    let view_role = api.engine.view_role(&caller.principal.user).await?;
    let email = users::email_of(&api, &caller).await?;
    Ok(Json(json!({
        "user": caller.principal.user,
        "is_owner": caller.is_owner,
        "memberships": caller.memberships,
        "via": caller.via,
        "needs_email": needs_email(email.as_deref()),
        "email": email,
        "email_verified": caller.email_verified,
        "must_set_password": caller.must_set_password,
        "must_enroll_two_step": caller.must_enroll_two_step,
        "view_role": view_role,
        "can_bypass": crate::bypass::can_bypass(&api, &caller).await?,
        "can_terminal": crate::groups::can_terminal(&api, &caller).await?,
    })))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SetViewRole {
    view_role: Option<String>,
}

/// `PUT /api/v1/me/view_role {view_role}`: the caller's own role view;
/// any signed-in member, viewers included.
pub(crate) async fn set_view_role(
    State(api): State<Api>,
    Extension(caller): Extension<Caller>,
    crate::routes::Body(input): crate::routes::Body<SetViewRole>,
) -> ApiResult<Json<Value>> {
    let role = input.view_role.as_deref();
    api.engine
        .set_view_role(&caller.principal.user, role)
        .await?;
    Ok(Json(json!({ "view_role": role })))
}

fn presented(headers: &HeaderMap) -> Option<&str> {
    let bearer = headers
        .get(AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "));
    bearer.or_else(|| cookie(headers, COOKIE_NAME))
}

/// The value of cookie `name`, if sent.
pub(crate) fn cookie<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers
        .get_all(COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(';'))
        .find_map(|pair| pair.trim().strip_prefix(name)?.strip_prefix('='))
}

/// Constant-time equality (length is not secret).
pub(crate) fn same(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auth_bearer_or_cookie_and_constant_time_compare() {
        let mut headers = HeaderMap::new();
        assert_eq!(presented(&headers), None);
        headers.insert(COOKIE, "a=1; sun_token=t0k".parse().unwrap());
        assert_eq!(presented(&headers), Some("t0k"));
        assert_eq!(cookie(&headers, "sun_tok"), None);
        headers.insert(AUTHORIZATION, "Bearer abc".parse().unwrap());
        assert_eq!(presented(&headers), Some("abc"));
        assert!(same(b"abc", b"abc"));
        assert!(!same(b"abc", b"abd"));
        assert!(!same(b"abc", b"abcd"));
    }
}
