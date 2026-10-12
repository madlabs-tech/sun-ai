//! Git connections and a workspace's repositories (like multica's Code tab).
//!
//! - `GET /workspaces/{id}/vcs`: `{connections, apps, can_manage}`, the
//!   workspace's connections (never a token or secret), which ecosystem git
//!   apps the operator set up (`{github, gitlab_oauth}` booleans) and
//!   whether the caller may connect and disconnect.
//! - `GET /workspaces/{id}/vcs/github/connect?return_to=` and
//!   `…/gitlab/connect?return_to=`: `{url, configured}`, the GitHub App
//!   install page or GitLab's authorize page (`url: null, configured:
//!   false` until the operator set the app's secrets), carrying a signed
//!   `state` that expires in 10 minutes and names the workspace, the
//!   person, the provider and where to land (`github` or `repositories`,
//!   which opens the picker). Its nonce also goes into an `HttpOnly;
//!   SameSite=Lax` cookie, so only the browser that started the flow can
//!   finish it.
//! - `GET /vcs/github/setup` and `GET /vcs/gitlab/callback` (open: the
//!   provider redirects the browser here): verify the state and the cookie
//!   (constant time), store the connection (using the state up: it
//!   connects once), and land on the UI's Code settings with
//!   `<provider>_connected=1` or `<provider>_error=<reason>`.
//! - `DELETE /vcs/connections/{id}` (mirrored PRs and their links stay),
//!   `POST /vcs/connections/{id}/rotate-webhook`.
//! - `GET /workspaces/{id}/vcs/{connection_id}/repos?q=&page=`: the picker.
//! - `GET|POST /workspaces/{id}/repos`, `PATCH /repos/{id}` and
//!   `DELETE /workspaces/{id}/repos/{repo_id}` (in `workspaces`): the
//!   workspace's list of `{url, description}`; no connection needed.
//!
//! Connection writes need `edit_workspace` and `manage_secrets`, repo
//! writes `edit_workspace` (layers in `routes.rs`); the workspace guard
//! answers 404 for another workspace's connection or repo.

use axum::Extension;
use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::header::SET_COOKIE;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::{Digest, Sha256};
use sun_core::model::{Permission, Repo, VcsConnection};
use sun_core::{Error, RepoId, Timestamp, VcsConnectionId, WorkspaceId};
use sun_engine::{AddedRepo, MAX_REPO_DESCRIPTION, RemoteRepo, Rotation, VcsApps};

use crate::Api;
use crate::auth::{Caller, cookie, redirect, same, secure};
use crate::error::{ApiError, ApiResult};
use crate::hosts::random_hex;
use crate::routes::{Body, check_branch, id};
use crate::webhooks::{hex, hmac};

/// `sun_vcs_state_<provider>`: one per provider, so a GitHub and a GitLab
/// connect started in two tabs both finish.
const STATE_COOKIE: &str = "sun_vcs_state";
const COOKIE_PATH: &str = "/api/v1/vcs";
/// From clicking Connect to coming back from the provider.
const STATE_TTL_S: i64 = 10 * 60;
/// Longest picker search.
const MAX_QUERY: usize = 100;

/// What a connect `state` carries. Signed, so none of it can be changed.
#[derive(Debug, Serialize, Deserialize)]
struct ConnectState {
    /// The workspace to connect.
    workspace: WorkspaceId,
    /// Who started it (the audit actor).
    actor: String,
    /// `github` or `gitlab`.
    provider: String,
    /// Unix seconds after which it is refused.
    expires: i64,
    /// Also in the browser's state cookie.
    nonce: String,
    /// Where the UI lands after: `github` (the Code tab) or `repositories`
    /// (the picker opens).
    #[serde(default)]
    return_to: Option<String>,
}

/// Landing targets a connect may ask for.
const RETURN_TARGETS: &[&str] = &["github", "repositories"];

impl ConnectState {
    /// What the engine uses up when it connects, so the state connects once.
    fn once(&self) -> (&str, Timestamp) {
        let expires = chrono::DateTime::from_timestamp(self.expires, 0);
        (&self.nonce, expires.unwrap_or_default())
    }
}

/// The state's HMAC key: derived from the kernel secret, for this use only.
fn state_key(api: &Api) -> Vec<u8> {
    Sha256::new()
        .chain_update(b"sun-vcs-state\0")
        .chain_update(api.config.host_secret.as_bytes())
        .finalize()
        .to_vec()
}

fn sign(api: &Api, state: &ConnectState) -> ApiResult<String> {
    let payload = serde_json::to_vec(state).map_err(|e| Error::Protocol(e.to_string()))?;
    let payload = URL_SAFE_NO_PAD.encode(payload);
    let mac = hex(&hmac::<Sha256>(&state_key(api), &[payload.as_bytes()]));
    Ok(format!("{payload}.{mac}"))
}

/// The state, when its signature is ours, it is for `provider`, it has not
/// expired and the browser holds its nonce. Constant-time compares.
fn verify(api: &Api, headers: &HeaderMap, token: &str, provider: &str) -> Option<ConnectState> {
    let (payload, mac) = token.split_once('.')?;
    let expected = hex(&hmac::<Sha256>(&state_key(api), &[payload.as_bytes()]));
    if !same(mac.as_bytes(), expected.as_bytes()) {
        return None;
    }
    let state: ConnectState =
        serde_json::from_slice(&URL_SAFE_NO_PAD.decode(payload).ok()?).ok()?;
    let nonce = cookie(headers, &state_cookie(provider))?;
    let fresh = state.expires > chrono::Utc::now().timestamp();
    (state.provider == provider && fresh && same(nonce.as_bytes(), state.nonce.as_bytes()))
        .then_some(state)
}

/// A new state for `workspace` and its nonce cookie.
fn start(
    api: &Api,
    workspace: WorkspaceId,
    caller: &Caller,
    provider: &str,
    return_to: Option<String>,
) -> ApiResult<(String, String)> {
    let state = ConnectState {
        workspace,
        actor: caller.actor(),
        provider: provider.into(),
        expires: chrono::Utc::now().timestamp() + STATE_TTL_S,
        nonce: random_hex()?,
        return_to,
    };
    // `Lax`: the provider's redirect back is a cross-site navigation.
    let cookie = format!(
        "{}={}; Path={COOKIE_PATH}; HttpOnly; SameSite=Lax; Max-Age={STATE_TTL_S}{}",
        state_cookie(provider),
        state.nonce,
        secure(api)
    );
    Ok((sign(api, &state)?, cookie))
}

fn state_cookie(provider: &str) -> String {
    format!("{STATE_COOKIE}_{provider}")
}

fn public_url(api: &Api) -> &str {
    api.config.public_url.trim_end_matches('/')
}

fn gitlab_redirect_uri(api: &Api) -> String {
    format!("{}/api/v1/vcs/gitlab/callback", public_url(api))
}

/// Where GitLab project hooks deliver: `<this>/<connection id>`.
fn hook_base(api: &Api) -> String {
    format!("{}/api/v1/webhooks/vcs", public_url(api))
}

/// The UI's Code settings, after a connect: `/#/settings/code?workspace=
/// <id>&<provider>_connected=1[&return_to=repositories]` or
/// `&<provider>_error=<reason>`.
fn landing(provider: &str, state: Option<&ConnectState>, outcome: Result<(), &str>) -> Response {
    let workspace = state.map_or_else(String::new, |s| format!("workspace={}&", s.workspace));
    let query = match outcome {
        Ok(()) => format!("{provider}_connected=1"),
        Err(reason) => format!("{provider}_error={reason}"),
    };
    let return_to = state
        .and_then(|s| s.return_to.as_deref())
        .filter(|r| *r == "repositories")
        .map_or_else(String::new, |r| format!("&return_to={r}"));
    let clear = format!(
        "{}=; Path={COOKIE_PATH}; HttpOnly; Max-Age=0",
        state_cookie(provider)
    );
    redirect(
        &format!("/#/settings/code?{workspace}{query}{return_to}"),
        vec![clear],
    )
}

/// The UI only gets a reason; the log gets the (redacted) error.
fn warn_failed(provider: &str, error: &Error) {
    let error = sun_core::redact::redact_known(&error.to_string());
    tracing::warn!("sun: {provider} connect failed: {error}");
}

/// Why a GitHub connect failed, for the UI (never the provider's own
/// words): multica's reasons.
fn reason(error: &Error) -> &'static str {
    match error {
        Error::NotFound(_) => "bad_workspace",
        Error::InvalidInput(_) | Error::Refused { .. } => "bad_installation_id",
        _ => "persist_failed",
    }
}

#[derive(Deserialize)]
pub(crate) struct ReturnTo {
    return_to: Option<String>,
}

impl ReturnTo {
    /// `github` (default) or `repositories`; anything else is 422.
    fn checked(self) -> ApiResult<Option<String>> {
        match self.return_to.as_deref() {
            None | Some("") => Ok(None),
            Some(target) if RETURN_TARGETS.contains(&target) => Ok(self.return_to),
            Some(_) => Err(ApiError::invalid(
                "return_to must be `github` or `repositories`.",
            )),
        }
    }
}

/// `{url, configured}`: the host's page for a new tab, or nothing until the
/// operator sets the app's secrets.
fn connect_answer(url: Option<String>, cookie: Option<String>) -> Response {
    let body = Json(json!({ "url": url, "configured": url.is_some() }));
    match cookie {
        Some(cookie) => ([(SET_COOKIE, cookie)], body).into_response(),
        None => body.into_response(),
    }
}

/// `GET /workspaces/{id}/vcs`: the connections, which ecosystem git apps
/// are set up (booleans, never a value) and whether the caller may connect
/// and disconnect (`edit_workspace` + `manage_secrets`).
#[derive(Serialize)]
pub(crate) struct Connections {
    connections: Vec<VcsConnection>,
    apps: VcsApps,
    can_manage: bool,
}

/// `GET /workspaces/{id}/vcs`.
pub(crate) async fn list(
    State(api): State<Api>,
    Extension(caller): Extension<Caller>,
    Path(workspace): Path<String>,
) -> ApiResult<Json<Connections>> {
    let workspace = id(&workspace, "workspace")?;
    Ok(Json(Connections {
        connections: api.engine.vcs_connections(workspace).await?,
        apps: api.engine.vcs_apps().await?,
        can_manage: caller.can(Permission::EditWorkspace) && caller.can(Permission::ManageSecrets),
    }))
}

/// `GET /workspaces/{id}/vcs/github/connect?return_to=`: `{url, configured}`
/// of the App's install page.
pub(crate) async fn github_connect(
    State(api): State<Api>,
    Extension(caller): Extension<Caller>,
    Path(workspace): Path<String>,
    Query(query): Query<ReturnTo>,
) -> ApiResult<Response> {
    let workspace = id(&workspace, "workspace")?;
    let return_to = query.checked()?;
    api.engine.workspace(workspace).await?;
    if !api.engine.vcs_apps().await?.github {
        return Ok(connect_answer(None, None));
    }
    let (state, cookie) = start(&api, workspace, &caller, "github", return_to)?;
    let url = api.engine.github_install_url(&state).await?;
    Ok(connect_answer(Some(url), Some(cookie)))
}

/// `GET /workspaces/{id}/vcs/gitlab/connect?return_to=`: `{url, configured}`
/// of GitLab's authorize page.
pub(crate) async fn gitlab_connect(
    State(api): State<Api>,
    Extension(caller): Extension<Caller>,
    Path(workspace): Path<String>,
    Query(query): Query<ReturnTo>,
) -> ApiResult<Response> {
    let workspace = id(&workspace, "workspace")?;
    let return_to = query.checked()?;
    api.engine.workspace(workspace).await?;
    if !api.engine.vcs_apps().await?.gitlab_oauth {
        return Ok(connect_answer(None, None));
    }
    let (state, cookie) = start(&api, workspace, &caller, "gitlab", return_to)?;
    let url = api
        .engine
        .gitlab_authorize_url(&gitlab_redirect_uri(&api), &state)
        .await?;
    Ok(connect_answer(Some(url), Some(cookie)))
}

#[derive(Deserialize)]
pub(crate) struct Setup {
    installation_id: Option<String>,
    state: Option<String>,
    setup_action: Option<String>,
}

/// `GET /vcs/github/setup?installation_id&state[&setup_action]`: GitHub
/// sends the browser back after an install (or a repo selection change).
///
/// The `installation_id` is trusted as GitHub sent it (no OAuth during
/// install, like multica): a deliberate user decision (2026-10-10). Someone
/// who knows another installation's id could connect it, but only a member
/// with `edit_workspace` + `manage_secrets` can start a connect at all.
pub(crate) async fn github_setup(
    State(api): State<Api>,
    headers: HeaderMap,
    Query(query): Query<Setup>,
) -> Response {
    let Some(raw) = query.state.as_deref().filter(|s| !s.is_empty()) else {
        return landing("github", None, Err("missing_params"));
    };
    let Some(state) = verify(&api, &headers, raw, "github") else {
        return landing("github", None, Err("invalid_state"));
    };
    // An org member asked the owner to install: nothing to store yet.
    if query.installation_id.is_none() && query.setup_action.as_deref() == Some("request") {
        return landing("github", Some(&state), Err("requested"));
    }
    let Some(raw_installation) = query.installation_id.as_deref().filter(|i| !i.is_empty()) else {
        return landing("github", Some(&state), Err("missing_params"));
    };
    let Some(installation) = raw_installation.parse::<i64>().ok().filter(|i| *i > 0) else {
        return landing("github", Some(&state), Err("bad_installation_id"));
    };
    match api
        .engine
        .connect_github(state.workspace, installation, &state.actor, state.once())
        .await
    {
        Ok(_) => landing("github", Some(&state), Ok(())),
        // The state already connected once: as invalid as a forged one.
        Err(Error::Forbidden(_)) => landing("github", None, Err("invalid_state")),
        Err(e) => {
            warn_failed("github", &e);
            landing("github", Some(&state), Err(reason(&e)))
        }
    }
}

#[derive(Deserialize)]
pub(crate) struct Callback {
    code: Option<String>,
    state: Option<String>,
    error: Option<String>,
}

/// `GET /vcs/gitlab/callback?code&state`: GitLab sends the browser back.
pub(crate) async fn gitlab_callback(
    State(api): State<Api>,
    headers: HeaderMap,
    Query(query): Query<Callback>,
) -> Response {
    let Some(raw) = query.state.as_deref().filter(|s| !s.is_empty()) else {
        return landing("gitlab", None, Err("missing_params"));
    };
    let Some(state) = verify(&api, &headers, raw, "gitlab") else {
        return landing("gitlab", None, Err("invalid_state"));
    };
    let code = query.code.filter(|c| (1..=512).contains(&c.len()));
    let (Some(code), None) = (code, query.error) else {
        return landing("gitlab", Some(&state), Err("denied"));
    };
    let webhook_secret = match random_hex() {
        Ok(s) => s,
        Err(_) => return landing("gitlab", Some(&state), Err("failed")),
    };
    let connected = api
        .engine
        .connect_gitlab_oauth(
            state.workspace,
            &code,
            &gitlab_redirect_uri(&api),
            &webhook_secret,
            &state.actor,
            state.once(),
        )
        .await;
    match connected {
        Ok(_) => landing("gitlab", Some(&state), Ok(())),
        Err(Error::Forbidden(_)) => landing("gitlab", None, Err("invalid_state")),
        Err(e) => {
            warn_failed("gitlab", &e);
            let reason = match e {
                Error::NotFound(_) => "bad_workspace",
                // GitLab answered 400/401 on `/oauth/token`: the operator's
                // client id or secret doesn't match the OAuth app.
                Error::InvalidInput(_) => "refused",
                Error::Unavailable(_) => "unreachable",
                _ => "failed",
            };
            landing("gitlab", Some(&state), Err(reason))
        }
    }
}

/// `DELETE /vcs/connections/{id}`: the mirrored PRs and their links stay.
pub(crate) async fn disconnect(
    State(api): State<Api>,
    Extension(caller): Extension<Caller>,
    Path(connection): Path<String>,
) -> ApiResult<StatusCode> {
    let connection: VcsConnectionId = id(&connection, "connection")?;
    api.engine
        .disconnect_vcs(connection, &caller.actor())
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

/// `POST /vcs/connections/{id}/rotate-webhook`: the new secret is in the
/// answer only when some hook needs it pasted by hand.
pub(crate) async fn rotate_webhook(
    State(api): State<Api>,
    Extension(caller): Extension<Caller>,
    Path(connection): Path<String>,
) -> ApiResult<Json<Rotation>> {
    let connection: VcsConnectionId = id(&connection, "connection")?;
    let rotation = api
        .engine
        .rotate_vcs_webhook(
            connection,
            &random_hex()?,
            &hook_base(&api),
            &caller.actor(),
        )
        .await?;
    Ok(Json(rotation))
}

#[derive(Deserialize)]
pub(crate) struct Picker {
    #[serde(default)]
    q: String,
    page: Option<u32>,
}

/// `GET /workspaces/{id}/vcs/{connection_id}/repos?q=&page=`.
pub(crate) async fn remote_repos(
    State(api): State<Api>,
    Path((workspace, connection)): Path<(String, String)>,
    Query(picker): Query<Picker>,
) -> ApiResult<Json<Vec<RemoteRepo>>> {
    let workspace: WorkspaceId = id(&workspace, "workspace")?;
    let connection: VcsConnectionId = id(&connection, "connection")?;
    let q = picker.q.trim();
    if q.chars().count() > MAX_QUERY || q.chars().any(char::is_control) {
        return Err(ApiError::invalid("Search with up to 100 characters."));
    }
    let page = picker.page.unwrap_or(1);
    if !(1..=100).contains(&page) {
        return Err(ApiError::invalid("Pages go from 1 to 100."));
    }
    Ok(Json(
        api.engine.vcs_repos(workspace, connection, q, page).await?,
    ))
}

fn check_description(description: &str) -> ApiResult<()> {
    match description.chars().count() <= MAX_REPO_DESCRIPTION {
        true => Ok(()),
        false => Err(ApiError::invalid(format!(
            "Keep the description under {MAX_REPO_DESCRIPTION} characters."
        ))),
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct NewRepo {
    url: String,
    default_branch: Option<String>,
    #[serde(default)]
    description: String,
}

/// `GET /workspaces/{id}/repos`: the workspace's repositories, oldest first.
pub(crate) async fn list_repos(
    State(api): State<Api>,
    Path(workspace): Path<String>,
) -> ApiResult<Json<Vec<Repo>>> {
    Ok(Json(api.engine.repos(id(&workspace, "workspace")?).await?))
}

/// `POST /workspaces/{id}/repos {url, default_branch?, description?}`: 201
/// with the repo (and, through GitLab, the hook to paste when Sun couldn't
/// make it); 422 "This repository is already added." for a URL on the list.
pub(crate) async fn add_repo(
    State(api): State<Api>,
    Extension(caller): Extension<Caller>,
    Path(workspace): Path<String>,
    Body(input): Body<NewRepo>,
) -> ApiResult<(StatusCode, Json<AddedRepo>)> {
    let workspace = id(&workspace, "workspace")?;
    let branch = input.default_branch.as_deref().map(str::trim);
    if let Some(branch) = branch {
        check_branch(branch)?;
    }
    check_description(&input.description)?;
    let added = api
        .engine
        .add_workspace_repo(
            workspace,
            &input.url,
            branch,
            &input.description,
            &hook_base(&api),
            &caller.actor(),
        )
        .await?;
    Ok((StatusCode::CREATED, Json(added)))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RepoChange {
    description: Option<String>,
    default_branch: Option<String>,
}

/// `PATCH /repos/{id} {description?, default_branch?}`.
pub(crate) async fn update_repo(
    State(api): State<Api>,
    Extension(caller): Extension<Caller>,
    Path(repo): Path<String>,
    Body(input): Body<RepoChange>,
) -> ApiResult<Json<Repo>> {
    let repo: RepoId = id(&repo, "repo")?;
    let branch = input.default_branch.as_deref().map(str::trim);
    if let Some(branch) = branch {
        check_branch(branch)?;
    }
    if let Some(description) = &input.description {
        check_description(description)?;
    }
    let repo = api
        .engine
        .update_repo(repo, input.description.as_deref(), branch, &caller.actor())
        .await?;
    Ok(Json(repo))
}
