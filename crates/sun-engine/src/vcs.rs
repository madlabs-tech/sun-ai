//! Git connections and repos, part of the **Facade** for the API: a
//! workspace connects GitHub (an App installation) or GitLab (OAuth) and
//! keeps its own list of repos (`{url, description}`), reached through the
//! connection on each repo's host when there is one. The providers are
//! **Adapters** (`github`, `gitlab`) over one small blocking HTTP client run
//! off the async runtime.
//!
//! Credentials never leave the kernel except to git and the git host mods:
//! - [`Vcs::credentials`] mints a GitHub installation token per call (cached
//!   until shortly before it expires) or reads a GitLab token, refreshing an
//!   OAuth one first when it is about to expire;
//! - [`call_git_host`] puts `{token, api_url}` into a git host mod call
//!   just before it is sent (never into a stored outbox row);
//! - `push` gives git the same token through its credential helper.
//!
//! Every minted or read token is remembered by `sun_core::redact`, so it is
//! redacted from events, logs and errors. Tokens and webhook secrets are
//! workspace-scoped [`Secret`](sun_core::model::Secret)s; a connection row
//! holds only their names. No transaction is held across an HTTP call.

mod github;
mod gitlab;

use std::collections::HashMap;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use serde::Serialize;
use serde_json::{Value, json};
use sun_core::model::{
    AuditAction, OutboxItem, OutboxStatus, Repo, SecretScope, VcsConnection, VcsProvider,
};
use sun_core::{
    Error, IdemKey, ModCall, ModLink, OutboxId, RepoId, Result, TaskId, Timestamp, VcsConnectionId,
    WorkspaceId,
};
use sun_store::{
    AuditRepo, OutboxRepo, RepoRepo, RunRepo, SecretRepo, SettingRepo, Store, TaskRepo, Tx,
    VcsRepo, WorkspaceRepo,
};

pub(crate) use github::pr_snapshot;
pub use github::{
    APP_API_URL as GITHUB_APP_API_URL, APP_ID as GITHUB_APP_ID,
    APP_PRIVATE_KEY as GITHUB_APP_PRIVATE_KEY, APP_SLUG as GITHUB_APP_SLUG,
    APP_WEBHOOK_SECRET as GITHUB_APP_WEBHOOK_SECRET,
};
pub use gitlab::{
    OAUTH_CLIENT_ID as GITLAB_OAUTH_CLIENT_ID, OAUTH_CLIENT_SECRET as GITLAB_OAUTH_CLIENT_SECRET,
    OAUTH_URL as GITLAB_OAUTH_URL,
};

use crate::Engine;
use crate::outbox::repo_slug;

const TIMEOUT: Duration = Duration::from_secs(20);
/// A cached GitHub token is replaced this long before it expires; a GitLab
/// OAuth token is refreshed this long before.
const REFRESH_EARLY: chrono::TimeDelta = chrono::TimeDelta::minutes(5);
/// Longest repo description.
pub const MAX_REPO_DESCRIPTION: usize = 1000;

/// Which ecosystem git apps an admin has set up, so the UI offers the right
/// connect flow. Read from the secrets' names only, never their values.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct VcsApps {
    /// The GitHub App: id, slug, private key and webhook secret are set.
    pub github: bool,
    /// GitLab sign-in: the OAuth client id and secret are set.
    pub gitlab_oauth: bool,
}

/// A repo the connection can reach, for the picker.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RemoteRepo {
    /// `owner/name` (`group/sub/name` on GitLab).
    pub name: String,
    /// HTTPS clone URL.
    pub url: String,
    /// The repo's page.
    pub web_url: String,
    /// Its default branch.
    pub default_branch: String,
    /// The host's own description.
    pub description: String,
    /// Not public.
    pub private: bool,
}

/// A GitLab hook a maintainer must add by hand: Sun could not (the
/// connected user isn't a maintainer of the project). Shown once.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ManualHook {
    /// The webhook URL.
    pub url: String,
    /// The secret token.
    pub secret: String,
}

/// A repo added to a workspace ([`Engine::add_workspace_repo`]).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct AddedRepo {
    /// The new row.
    pub repo: Repo,
    /// Set when the GitLab hook could not be made.
    pub manual_hook: Option<ManualHook>,
}

/// A rotated GitLab webhook secret ([`Engine::rotate_vcs_webhook`]).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Rotation {
    /// Hooks updated with the new secret.
    pub updated: usize,
    /// Repos whose hook could not be updated: a maintainer pastes
    /// `manual_hook` into them by hand.
    pub manual_repos: Vec<String>,
    /// Set when `manual_repos` isn't empty.
    pub manual_hook: Option<ManualHook>,
}

/// What a git host mod or git gets for one call.
pub(crate) struct Credentials {
    pub(crate) token: String,
    /// The REST API base (`https://api.github.com`, `<gitlab>/api/v4`).
    pub(crate) api_url: String,
}

/// Token minting and refreshing, shared by the engine, the executor
/// (pushes) and the outbox worker (mod calls). Cheap to clone.
#[derive(Clone)]
pub(crate) struct Vcs {
    store: Store,
    /// GitHub installation tokens by installation, until they expire.
    github_tokens: Arc<Mutex<HashMap<i64, (String, Timestamp)>>>,
    // ponytail: one lock for every GitLab refresh; per connection if many
    // workspaces refresh at once.
    refreshing: Arc<tokio::sync::Mutex<()>>,
}

impl Vcs {
    pub(crate) fn new(store: Store) -> Self {
        Self {
            store,
            github_tokens: Arc::default(),
            refreshing: Arc::default(),
        }
    }

    /// A token and API base for `connection`, valid for at least a few
    /// minutes. `force` (after a 401) mints a new GitHub token or refreshes
    /// a GitLab OAuth one even if it looks fresh.
    pub(crate) async fn credentials(
        &self,
        connection: &VcsConnection,
        force: bool,
    ) -> Result<Credentials> {
        match connection.provider {
            VcsProvider::Github => {
                let app = github::App::load(&self.store).await?;
                let installation = connection.installation_id.ok_or_else(|| {
                    Error::InvalidInput("the GitHub connection has no installation".into())
                })?;
                if force {
                    self.github_tokens
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner)
                        .remove(&installation);
                }
                Ok(Credentials {
                    token: self.github_token(&app, installation).await?,
                    api_url: app.api,
                })
            }
            VcsProvider::Gitlab => Ok(Credentials {
                token: self.gitlab_token(connection, force).await?,
                api_url: gitlab::api(&connection.instance_url),
            }),
        }
    }

    /// Whether [`Self::credentials`] with `force` can give `connection` a
    /// new token (a GitLab access token can't be refreshed).
    fn refreshable(connection: &VcsConnection) -> bool {
        connection.provider == VcsProvider::Github || connection.refresh_secret.is_some()
    }

    async fn github_token(&self, app: &github::App, installation: i64) -> Result<String> {
        let tokens = || {
            self.github_tokens
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
        };
        let fresh = chrono::Utc::now() + REFRESH_EARLY;
        if let Some((token, _)) = tokens()
            .get(&installation)
            .filter(|(_, expires)| *expires > fresh)
        {
            return Ok(token.clone());
        }
        let (token, expires) = app.installation_token(installation).await?;
        tokens().insert(installation, (token.clone(), expires));
        Ok(token)
    }

    /// The connection's GitLab token; an OAuth one is refreshed first when
    /// it expires soon, or always with `force` (after a 401). The rotated
    /// refresh token, the access token and the expiry commit together.
    async fn gitlab_token(&self, connection: &VcsConnection, force: bool) -> Result<String> {
        let scope = SecretScope::Workspace(connection.workspace_id);
        let name = connection
            .token_secret
            .clone()
            .ok_or_else(|| Error::InvalidInput("the GitLab connection has no token".into()))?;
        let due = |c: &VcsConnection| {
            c.refresh_secret.is_some()
                && (force
                    || c.token_expires_at
                        .is_none_or(|at| at <= chrono::Utc::now() + REFRESH_EARLY))
        };
        if !due(connection) {
            return Ok(self.store.read().await?.secret_value(&scope, &name).await?);
        }
        let _one_at_a_time = self.refreshing.lock().await;
        // Another call may have refreshed while this one waited. A
        // disconnected connection (its hooks still being deleted) has no
        // row left: its secrets are what the caller named.
        let current = match self.store.read().await?.get_connection(connection.id).await {
            Ok(current) => current,
            Err(e) => match Error::from(e) {
                Error::NotFound(_) => connection.clone(),
                e => return Err(e),
            },
        };
        if !force && !due(&current) {
            return Ok(self.store.read().await?.secret_value(&scope, &name).await?);
        }
        let Some(refresh_name) = current.refresh_secret.clone() else {
            return Ok(self.store.read().await?.secret_value(&scope, &name).await?);
        };
        let refresh = self
            .store
            .read()
            .await?
            .secret_value(&scope, &refresh_name)
            .await?;
        let grant = gitlab::OAuth::load(&self.store)
            .await?
            .refresh(&refresh, current.oauth_redirect_uri.as_deref())
            .await?;
        let mut tx = self.store.begin().await?;
        tx.set_secret(&scope, &name, &grant.access_token).await?;
        tx.set_secret(&scope, &refresh_name, &grant.refresh_token)
            .await?;
        tx.set_token_expiry(current.id, Some(grant.expires_at))
            .await?;
        tx.commit().await?;
        Ok(grant.access_token)
    }

    /// One GitLab API call on `connection`, retried once after a refresh
    /// when an OAuth token gets a 401.
    async fn gitlab(
        &self,
        connection: &VcsConnection,
        method: &'static str,
        path: &str,
        body: Option<Value>,
    ) -> Result<Reply> {
        let url = format!("{}{path}", gitlab::api(&connection.instance_url));
        let token = self.gitlab_token(connection, false).await?;
        let reply = send(
            method,
            url.clone(),
            Some(token),
            body.clone().map(Body::Json),
        )
        .await?;
        if reply.status != 401 || connection.refresh_secret.is_none() {
            return Ok(reply);
        }
        let token = self.gitlab_token(connection, true).await?;
        send(method, url, Some(token), body.map(Body::Json)).await
    }
}

/// A request body.
pub(crate) enum Body {
    Json(Value),
    Form(Vec<(&'static str, String)>),
}

impl From<Value> for Body {
    fn from(value: Value) -> Self {
        Body::Json(value)
    }
}

/// A host's answer.
pub(crate) struct Reply {
    pub(crate) status: u16,
    pub(crate) body: Value,
}

impl Reply {
    /// The body of a 2xx; else a 4xx is `InvalidInput` (the person can fix
    /// it), anything else `Unavailable`. `what` names the call without its
    /// ids or query, and the message is redacted.
    pub(crate) fn check(self, host: &str, what: &str) -> Result<Value> {
        if (200..300).contains(&self.status) {
            return Ok(self.body);
        }
        let said = ["message", "error_description", "error"]
            .iter()
            .find_map(|k| self.body.get(*k))
            .map(|m| m.as_str().map_or_else(|| m.to_string(), str::to_owned))
            .unwrap_or_default();
        let message = sun_core::redact::redact_known(&format!(
            "{host} answered {} to {what}: {}",
            self.status,
            said.chars().take(300).collect::<String>()
        ));
        Err(match self.status {
            400..=499 if self.status != 429 => Error::InvalidInput(message),
            _ => Error::Unavailable(message),
        })
    }
}

/// One HTTP call with a JSON answer. `token` goes as `Authorization:
/// Bearer` and never into a message. Redirects are not followed (the URL
/// may come from a person: no hops to other hosts).
pub(crate) async fn send(
    method: &'static str,
    url: String,
    token: Option<String>,
    body: Option<Body>,
) -> Result<Reply> {
    let what = url
        .split('?')
        .next()
        .unwrap_or_default()
        .split_once("://")
        .map_or("", |(_, rest)| rest)
        .to_owned();
    tokio::task::spawn_blocking(move || {
        let agent = ureq::Agent::config_builder()
            .http_status_as_error(false)
            .timeout_global(Some(TIMEOUT))
            .max_redirects(0)
            .build()
            .new_agent();
        let mut request = ureq::http::Request::builder()
            .method(method)
            .uri(&url)
            .header("accept", "application/json")
            .header("user-agent", "sun")
            .header("x-github-api-version", "2022-11-28");
        if let Some(token) = &token {
            request = request.header("authorization", format!("Bearer {token}"));
        }
        let unreachable = |e: &dyn std::fmt::Display| {
            Error::Unavailable(sun_core::redact::redact_known(&format!(
                "{method} {what}: {e}"
            )))
        };
        let sent = match body {
            None => request.body(ureq::SendBody::none()).map(|r| agent.run(r)),
            Some(Body::Json(value)) => request
                .header("content-type", "application/json")
                .body(value.to_string().into_bytes())
                .map(|r| agent.run(r)),
            Some(Body::Form(fields)) => {
                let form: Vec<String> = fields
                    .iter()
                    .map(|(k, v)| format!("{k}={}", encode(v)))
                    .collect();
                request
                    .header("content-type", "application/x-www-form-urlencoded")
                    .body(form.join("&").into_bytes())
                    .map(|r| agent.run(r))
            }
        };
        let mut response = sent
            .map_err(|e| unreachable(&e))?
            .map_err(|e| unreachable(&e))?;
        let text = response
            .body_mut()
            .read_to_string()
            .map_err(|e| unreachable(&e))?;
        Ok(Reply {
            status: response.status().as_u16(),
            body: serde_json::from_str(&text).unwrap_or(Value::Null),
        })
    })
    .await
    .map_err(|_| Error::Unavailable("http call panicked".into()))?
}

/// Percent-encodes a path segment or query value.
pub(crate) fn encode(s: &str) -> String {
    s.bytes()
        .map(
            |b| match b.is_ascii_alphanumeric() || b"-._~".contains(&b) {
                true => (b as char).to_string(),
                false => format!("%{b:02X}"),
            },
        )
        .collect()
}

/// An instance or API URL without its trailing `/`: `https://host[:port][/path]`,
/// or plain `http` to localhost in debug builds (tests, a local GitLab in
/// dev). No credentials, query or fragment.
pub fn check_url(url: &str) -> Result<String> {
    let url = url.trim().trim_end_matches('/');
    let rest = url.strip_prefix("https://").or_else(|| {
        url.strip_prefix("http://")
            .filter(|_| cfg!(debug_assertions))
    });
    let host = rest.map(|r| r.split(['/', ':']).next().unwrap_or_default());
    let local = matches!(host, Some("localhost" | "127.0.0.1"));
    let ok = match (rest, host) {
        (Some(rest), Some(host)) => {
            url.len() <= 2048
                && !host.is_empty()
                && (url.starts_with("https://") || local)
                && !rest.contains(['@', '?', '#', ' ', '\\'])
                && host
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"-.".contains(&b))
        }
        _ => false,
    };
    match ok {
        true => Ok(url.to_owned()),
        false => Err(Error::InvalidInput(format!(
            "`{url}` is not an https URL of a git host."
        ))),
    }
}

/// The host (`host[:port]`, lowercase) of a clone URL: `https://host/…`,
/// `ssh://git@host/…` or `git@host:…`.
pub(crate) fn host_of(url: &str) -> Option<String> {
    let host = match url.split_once("://") {
        Some((_, rest)) => rest.split('/').next()?.rsplit('@').next()?,
        None => url.split_once(':')?.0.rsplit('@').next()?,
    };
    // An ssh port is not the web host's.
    let host = match url.starts_with("ssh://") {
        true => host.split(':').next()?,
        false => host,
    };
    (!host.is_empty()).then(|| host.to_lowercase())
}

/// Ecosystem secret `name`, or `None`.
pub(crate) async fn ecosystem_secret(store: &Store, name: &str) -> Result<Option<String>> {
    let scope = crate::ecosystem_scope()?;
    match store.read().await?.secret_value(&scope, name).await {
        Ok(value) => Ok(Some(value)),
        Err(e) => match Error::from(e) {
            Error::NotFound(_) => Ok(None),
            e => Err(e),
        },
    }
}

/// The workspace connection a git host mod `mod_name` (`github`, `gitlab`)
/// should use for `repos` (the one named `slug`, else the first): the
/// connection the repo was added through, else the one on its host.
pub(crate) async fn connection_for(
    tx: &mut Tx,
    workspace: WorkspaceId,
    repos: &[Repo],
    mod_name: &str,
    slug: Option<&str>,
) -> Result<Option<VcsConnection>> {
    let Ok(provider) = mod_name.parse::<VcsProvider>() else {
        return Ok(None);
    };
    let repo = slug
        .and_then(|slug| repos.iter().find(|r| repo_slug(&r.url) == slug))
        .or_else(|| repos.first());
    let Some(repo) = repo else {
        return Ok(None);
    };
    Ok(connection_of_repo(tx, workspace, repo)
        .await?
        .filter(|c| c.provider == provider))
}

/// The connection `workspace` reaches `repo` through: the one on its host
/// (any provider), if any.
pub(crate) async fn connection_of_repo(
    tx: &mut Tx,
    workspace: WorkspaceId,
    repo: &Repo,
) -> Result<Option<VcsConnection>> {
    let host = host_of(&repo.url);
    Ok(tx
        .connections(workspace)
        .await?
        .into_iter()
        .find(|c| host.is_some() && host_of(&c.instance_url) == host))
}

/// The repos of `connection`'s workspace on its host that carry a project
/// hook Sun made.
async fn hooked_repos(tx: &mut Tx, connection: &VcsConnection) -> Result<Vec<Repo>> {
    let host = host_of(&connection.instance_url);
    Ok(tx
        .repos(connection.workspace_id)
        .await?
        .into_iter()
        .filter(|r| r.hook_id.is_some() && host.is_some() && host_of(&r.url) == host)
        .collect())
}

/// The workspace a task is on.
async fn workspace_of_task(tx: &mut Tx, task: TaskId) -> Result<WorkspaceId> {
    Ok(tx.get_task(task).await?.workspace_id)
}

/// Sends a git host mod call for `task` with the task workspace's
/// connection token added as `credentials: {token, api_url}` (none: the mod
/// keeps its own env token). The params with credentials exist only here,
/// never in a stored row. When the mod's host answers 401, the token is
/// forced fresh once and the call sent once more: a refused call took no
/// effect, so the retry can't apply it twice (and it keeps its idem key).
pub(crate) async fn call_git_host(
    store: &Store,
    vcs: &Vcs,
    mods: &dyn ModLink,
    task: Option<TaskId>,
    call: ModCall,
) -> Result<Value> {
    let connection = match task {
        Some(task) => connection_of_call(store, task, &call).await?,
        None => None,
    };
    let Some(connection) = connection else {
        return mods.call(call).await;
    };
    let with = |credentials: Credentials| {
        let mut call = call.clone();
        call.params["credentials"] =
            json!({ "token": credentials.token, "api_url": credentials.api_url });
        call
    };
    let first = mods
        .call(with(vcs.credentials(&connection, false).await?))
        .await;
    match first {
        Err(e) if refused(&e) && Vcs::refreshable(&connection) => {
            mods.call(with(vcs.credentials(&connection, true).await?))
                .await
        }
        other => other,
    }
}

/// A git host mod's error for an HTTP 401 from its host: the status comes
/// as data (`sun-mod-sdk` sets `data.http_status` on every HTTP error).
fn refused(error: &Error) -> bool {
    matches!(
        error,
        Error::Refused {
            http_status: 401,
            ..
        }
    )
}

/// The workspace connection a git host mod call for `task` uses.
async fn connection_of_call(
    store: &Store,
    task: TaskId,
    call: &ModCall,
) -> Result<Option<VcsConnection>> {
    if call.mod_name.parse::<VcsProvider>().is_err() || !call.params.is_object() {
        return Ok(None);
    }
    // A reconcile carries the original call under `params`.
    let slug = call.params["repo"]
        .as_str()
        .or_else(|| call.params["params"]["repo"].as_str())
        .map(str::to_owned);
    let mut tx = store.read().await?;
    let workspace = workspace_of_task(&mut tx, task).await?;
    let run = tx.runs_of_task(task).await?.pop();
    let mut repos = Vec::new();
    for id in run.map(|r| r.repo_ids).unwrap_or_default() {
        repos.push(tx.get_repo(id).await?);
    }
    connection_for(&mut tx, workspace, &repos, &call.mod_name, slug.as_deref()).await
}

/// The secret names of a GitLab connection `id`.
fn secret_names(id: VcsConnectionId) -> (String, String, String) {
    let prefix = sun_core::model::Secret::CONNECTION_PREFIX;
    (
        format!("{prefix}{id}_TOKEN"),
        format!("{prefix}{id}_REFRESH"),
        format!("{prefix}{id}_WEBHOOK"),
    )
}

/// A connection's name in audit rows, e.g. `GitHub acme`.
fn label(connection: &VcsConnection) -> String {
    let host = match connection.provider {
        VcsProvider::Github => "GitHub",
        VcsProvider::Gitlab => "GitLab",
    };
    format!("{host} {}", connection.account_login)
}

/// A GitLab connection to save ([`Engine::save_gitlab`]).
struct NewGitlab<'a> {
    workspace: WorkspaceId,
    instance_url: String,
    login: String,
    token: &'a str,
    refresh: Option<(&'a str, Timestamp)>,
    /// OAuth: the redirect URI the code was issued for.
    redirect_uri: Option<&'a str>,
    webhook_secret: &'a str,
    actor: &'a str,
    /// OAuth: the connect state, used up here ([`use_state`]).
    state: Option<ConnectState<'a>>,
}

/// A connect state's nonce and expiry. It is used up in the transaction
/// that stores the connection, so one state connects once.
pub type ConnectState<'a> = (&'a str, Timestamp);

/// Uses up `state` in `tx`: `Forbidden` when it already connected.
async fn use_state(tx: &mut Tx, (nonce, expires): ConnectState<'_>) -> Result<()> {
    if tx.use_once(&format!("vcs_state:{nonce}"), expires).await? {
        Ok(())
    } else {
        Err(Error::Forbidden(
            "this connect state was already used".into(),
        ))
    }
}

impl Engine {
    /// A workspace's connections (no token, no secret), oldest first.
    pub async fn vcs_connections(&self, workspace: WorkspaceId) -> Result<Vec<VcsConnection>> {
        let mut tx = self.store.read().await?;
        tx.get_workspace(workspace).await?;
        Ok(tx.connections(workspace).await?)
    }

    /// Which git apps are set up (names of ecosystem secrets only).
    pub async fn vcs_apps(&self) -> Result<VcsApps> {
        let scope = crate::ecosystem_scope()?;
        let secrets = self.store.read().await?.list_secrets(Some(&scope)).await?;
        let set = |names: &[&str]| names.iter().all(|n| secrets.iter().any(|s| s.name == *n));
        Ok(VcsApps {
            github: set(github::REQUIRED),
            gitlab_oauth: set(gitlab::OAUTH_REQUIRED),
        })
    }

    /// One connection, or `NotFound`.
    pub async fn vcs_connection(&self, id: VcsConnectionId) -> Result<VcsConnection> {
        Ok(self.store.read().await?.get_connection(id).await?)
    }

    /// The GitHub App's install page, carrying `state` (signed by the API).
    pub async fn github_install_url(&self, state: &str) -> Result<String> {
        github::install_url(&self.store, state).await
    }

    /// GitLab's authorize page for the OAuth app, back to `redirect_uri`.
    pub async fn gitlab_authorize_url(&self, redirect_uri: &str, state: &str) -> Result<String> {
        Ok(gitlab::OAuth::load(&self.store)
            .await?
            .authorize_url(redirect_uri, state))
    }

    /// The GitHub App's webhook secret, or `None` until an admin set it.
    pub async fn github_webhook_secret(&self) -> Result<Option<String>> {
        ecosystem_secret(&self.store, github::APP_WEBHOOK_SECRET).await
    }

    /// The webhook secret of GitLab connection `id`; `None` for any other
    /// id (unknown, GitHub), so a forged URL is just unverified.
    pub async fn vcs_hook_secret(&self, id: VcsConnectionId) -> Result<Option<String>> {
        let mut tx = self.store.read().await?;
        let connection = match tx.get_connection(id).await {
            Ok(c) => c,
            Err(e) => match Error::from(e) {
                Error::NotFound(_) => return Ok(None),
                e => return Err(e),
            },
        };
        match (&connection.provider, &connection.webhook_secret) {
            (VcsProvider::Gitlab, Some(name)) => {
                let scope = SecretScope::Workspace(connection.workspace_id);
                Ok(Some(tx.secret_value(&scope, name).await?))
            }
            _ => Ok(None),
        }
    }

    /// Connects `workspace` to GitHub App installation `installation` (an
    /// unknown installation is `InvalidInput`). Reconnecting replaces the
    /// workspace's GitHub connection in place. `state` connects once (then
    /// `Forbidden`).
    pub async fn connect_github(
        &self,
        workspace: WorkspaceId,
        installation: i64,
        actor: &str,
        state: ConnectState<'_>,
    ) -> Result<VcsConnection> {
        let app = github::App::load(&self.store).await?;
        let login = app.installation_account(installation).await?;
        let mut tx = self.store.begin().await?;
        use_state(&mut tx, state).await?;
        let name = tx.get_workspace(workspace).await?.name;
        let existing = existing(&mut tx, workspace, VcsProvider::Github, github::WEB).await?;
        let connection = VcsConnection {
            id: existing
                .as_ref()
                .map_or_else(VcsConnectionId::generate, |c| c.id),
            workspace_id: workspace,
            provider: VcsProvider::Github,
            instance_url: github::WEB.into(),
            account_login: login,
            installation_id: Some(installation),
            token_secret: None,
            refresh_secret: None,
            token_expires_at: None,
            oauth_redirect_uri: None,
            webhook_secret: None,
            connected_by: actor.into(),
            created_at: existing.map_or_else(chrono::Utc::now, |c| c.created_at),
        };
        tx.put_connection(&connection).await?;
        let detail = format!("connected {}", label(&connection));
        tx.append_audit(actor, AuditAction::WorkspaceChange, &name, &detail)
            .await?;
        tx.commit().await?;
        Ok(connection)
    }

    /// Finishes GitLab OAuth: trades `code` for tokens and connects
    /// `workspace` to the OAuth app's GitLab. `webhook_secret` (fresh
    /// random) signs the project hooks, unless the connection had one.
    /// `state` connects once (then `Forbidden`).
    pub async fn connect_gitlab_oauth(
        &self,
        workspace: WorkspaceId,
        code: &str,
        redirect_uri: &str,
        webhook_secret: &str,
        actor: &str,
        state: ConnectState<'_>,
    ) -> Result<VcsConnection> {
        let oauth = gitlab::OAuth::load(&self.store).await?;
        let grant = oauth.exchange(code, redirect_uri).await?;
        let login = gitlab::username(&oauth.url, &grant.access_token).await?;
        self.save_gitlab(NewGitlab {
            workspace,
            instance_url: oauth.url,
            login,
            token: &grant.access_token,
            refresh: Some((&grant.refresh_token, grant.expires_at)),
            redirect_uri: Some(redirect_uri),
            webhook_secret,
            actor,
            state: Some(state),
        })
        .await
    }

    /// Stores a GitLab connection and its secrets in one transaction.
    async fn save_gitlab(&self, new: NewGitlab<'_>) -> Result<VcsConnection> {
        let scope = SecretScope::Workspace(new.workspace);
        let mut tx = self.store.begin().await?;
        if let Some(state) = new.state {
            use_state(&mut tx, state).await?;
        }
        let name = tx.get_workspace(new.workspace).await?.name;
        let existing = existing(
            &mut tx,
            new.workspace,
            VcsProvider::Gitlab,
            &new.instance_url,
        )
        .await?;
        let id = existing
            .as_ref()
            .map_or_else(VcsConnectionId::generate, |c| c.id);
        let (token_name, refresh_name, webhook_name) = secret_names(id);
        tx.set_secret(&scope, &token_name, new.token).await?;
        match new.refresh {
            Some((refresh, _)) => {
                tx.set_secret(&scope, &refresh_name, refresh).await?;
            }
            None => {
                tx.delete_secret(&scope, &refresh_name).await?;
            }
        }
        // The project hooks already carry the old secret: keep it.
        if existing.as_ref().is_none_or(|c| c.webhook_secret.is_none()) {
            tx.set_secret(&scope, &webhook_name, new.webhook_secret)
                .await?;
        }
        let connection = VcsConnection {
            id,
            workspace_id: new.workspace,
            provider: VcsProvider::Gitlab,
            instance_url: new.instance_url,
            account_login: new.login,
            installation_id: None,
            token_secret: Some(token_name),
            refresh_secret: new.refresh.map(|_| refresh_name),
            token_expires_at: new.refresh.map(|(_, at)| at),
            oauth_redirect_uri: new.redirect_uri.map(str::to_owned),
            webhook_secret: Some(webhook_name),
            connected_by: new.actor.into(),
            created_at: existing.map_or_else(chrono::Utc::now, |c| c.created_at),
        };
        tx.put_connection(&connection).await?;
        let detail = format!("connected {}", label(&connection));
        tx.append_audit(new.actor, AuditAction::WorkspaceChange, &name, &detail)
            .await?;
        tx.commit().await?;
        Ok(connection)
    }

    /// Disconnects: deletes the GitLab hooks Sun made (best effort), the
    /// connection's secrets and the row. Its repos stay, without it.
    pub async fn disconnect_vcs(&self, id: VcsConnectionId, actor: &str) -> Result<()> {
        let mut tx = self.store.begin().await?;
        let connection = tx.get_connection(id).await?;
        let name = tx.get_workspace(connection.workspace_id).await?.name;
        // The hooks go through the outbox, with the token they need: its
        // secrets stay until the last one is deleted ([`delete_hook`]).
        let hooks = hooked_repos(&mut tx, &connection).await?;
        for repo in &hooks {
            if let Some(hook) = repo.hook_id {
                tx.enqueue(&hook_row(&connection, &repo.url, hook)?).await?;
                tx.set_hook(repo.id, None).await?;
            }
        }
        if hooks.is_empty() {
            forget_secrets(&mut tx, &connection).await?;
        }
        tx.delete_connection(id).await?;
        let detail = format!("disconnected {}", label(&connection));
        tx.append_audit(actor, AuditAction::WorkspaceChange, &name, &detail)
            .await?;
        tx.commit().await?;
        self.outbox_wake.notify_one();
        Ok(())
    }

    /// GitHub uninstalled or suspended the App on installation
    /// `installation` (`installation` deliveries with action `deleted` or
    /// `suspend`, [`Engine::github_delivery`]): every connection to it goes.
    pub async fn forget_github_installation(&self, installation: i64) -> Result<()> {
        let connections = self
            .store
            .read()
            .await?
            .connections_of_installation(installation)
            .await?;
        for connection in connections {
            self.disconnect_vcs(connection.id, "github").await?;
        }
        Ok(())
    }

    /// Gives GitLab connection `id` a new webhook secret (`secret`, fresh
    /// random) and updates every hook Sun made with it.
    pub async fn rotate_vcs_webhook(
        &self,
        id: VcsConnectionId,
        secret: &str,
        hook_base: &str,
        actor: &str,
    ) -> Result<Rotation> {
        let connection = self.vcs_connection(id).await?;
        let (VcsProvider::Gitlab, Some(name)) = (&connection.provider, &connection.webhook_secret)
        else {
            return Err(Error::InvalidInput(
                "GitHub webhooks are signed with the App's secret: rotate it on GitHub.".into(),
            ));
        };
        let scope = SecretScope::Workspace(connection.workspace_id);
        let mut tx = self.store.begin().await?;
        let workspace = tx.get_workspace(connection.workspace_id).await?.name;
        tx.set_secret(&scope, name, secret).await?;
        let detail = format!("rotated the webhook secret of {}", label(&connection));
        tx.append_audit(actor, AuditAction::WorkspaceChange, &workspace, &detail)
            .await?;
        tx.commit().await?;
        let url = format!("{hook_base}/{id}");
        let hooks = hooked_repos(&mut self.store.read().await?, &connection).await?;
        let mut rotation = Rotation {
            updated: 0,
            manual_repos: Vec::new(),
            manual_hook: None,
        };
        for repo in hooks {
            let Some(hook) = repo.hook_id else { continue };
            let path = format!(
                "{}/hooks/{hook}",
                gitlab::project_path(&repo_slug(&repo.url))
            );
            let body = gitlab::hook(&url, secret);
            match self.vcs.gitlab(&connection, "PUT", &path, Some(body)).await {
                Ok(reply) if (200..300).contains(&reply.status) => rotation.updated += 1,
                _ => rotation.manual_repos.push(repo.url),
            }
        }
        if !rotation.manual_repos.is_empty() {
            rotation.manual_hook = Some(ManualHook {
                url,
                secret: secret.into(),
            });
        }
        Ok(rotation)
    }

    /// One page (from 1) of the repos connection `id` reaches, for the
    /// picker; `NotFound` unless it is `workspace`'s.
    pub async fn vcs_repos(
        &self,
        workspace: WorkspaceId,
        id: VcsConnectionId,
        query: &str,
        page: u32,
    ) -> Result<Vec<RemoteRepo>> {
        let connection = self.workspace_connection(workspace, id).await?;
        match connection.provider {
            VcsProvider::Github => {
                let credentials = self.vcs.credentials(&connection, false).await?;
                github::repos(&credentials.api_url, &credentials.token, query, page).await
            }
            VcsProvider::Gitlab => {
                let path = gitlab::projects_path(query, page);
                let body = self
                    .vcs
                    .gitlab(&connection, "GET", &path, None)
                    .await?
                    .check("GitLab", "GET /projects")?;
                Ok(gitlab::projects(&body))
            }
        }
    }

    /// `workspace`'s connection `id`; another workspace's is `NotFound`.
    async fn workspace_connection(
        &self,
        workspace: WorkspaceId,
        id: VcsConnectionId,
    ) -> Result<VcsConnection> {
        let connection = self.vcs_connection(id).await?;
        match connection.workspace_id == workspace {
            true => Ok(connection),
            false => Err(Error::NotFound(format!("git connection {id}"))),
        }
    }

    /// Adds the repo at `url` to `workspace`'s list (unique by URL there:
    /// "This repository is already added."). A missing `default_branch` is
    /// read from the host when the workspace has a connection on it, else
    /// `main`. Through a GitLab connection a project hook is made
    /// (`hook_base/<connection>`); when GitLab refuses, the repo is still
    /// added and [`AddedRepo::manual_hook`] says what to paste.
    pub async fn add_workspace_repo(
        &self,
        workspace: WorkspaceId,
        url: &str,
        default_branch: Option<&str>,
        description: &str,
        hook_base: &str,
        actor: &str,
    ) -> Result<AddedRepo> {
        let url = url.trim();
        check_repo_url(url)?;
        let slug = repo_slug(url);
        let connection = {
            let mut tx = self.store.read().await?;
            tx.get_workspace(workspace).await?;
            if tx.repo_by_url(workspace, url).await?.is_some() {
                return Err(Error::InvalidInput(
                    "This repository is already added.".into(),
                ));
            }
            let host = host_of(url);
            tx.connections(workspace)
                .await?
                .into_iter()
                .find(|c| host.is_some() && host_of(&c.instance_url) == host)
        };
        let default_branch = match (default_branch, &connection) {
            (Some(branch), _) => branch.to_owned(),
            (None, None) => "main".to_owned(),
            (None, Some(connection)) => match connection.provider {
                VcsProvider::Github => {
                    let credentials = self.vcs.credentials(connection, false).await?;
                    github::repo(&credentials.api_url, &credentials.token, &slug)
                        .await?
                        .default_branch
                }
                VcsProvider::Gitlab => {
                    let path = gitlab::project_path(&slug);
                    let reply = self.vcs.gitlab(connection, "GET", &path, None).await?;
                    if reply.status == 404 {
                        return Err(Error::InvalidInput(format!(
                            "{} can't see {slug} on GitLab.",
                            connection.account_login
                        )));
                    }
                    gitlab::project(&reply.check("GitLab", "GET /projects/:id")?)?.default_branch
                }
            },
        };
        let repo = Repo {
            id: RepoId::generate(),
            workspace_id: workspace,
            url: url.to_owned(),
            default_branch,
            gate_config: json!({}),
            description: description.trim().to_owned(),
            hook_id: None,
        };
        let mut tx = self.store.begin().await?;
        let name = tx.get_workspace(workspace).await?.name;
        tx.insert_repo(&repo).await?;
        let detail = format!("added repo {}", repo.url);
        tx.append_audit(actor, AuditAction::WorkspaceChange, &name, &detail)
            .await?;
        tx.commit().await?;
        let mut added = AddedRepo {
            repo,
            manual_hook: None,
        };
        if let Some(connection) = connection.filter(|c| c.provider == VcsProvider::Gitlab) {
            let hook_url = format!("{hook_base}/{}", connection.id);
            let secret = self.vcs_hook_secret(connection.id).await?.ok_or_else(|| {
                Error::InvalidInput("the GitLab connection has no webhook secret".into())
            })?;
            let path = format!("{}/hooks", gitlab::project_path(&slug));
            let made = self
                .vcs
                .gitlab(
                    &connection,
                    "POST",
                    &path,
                    Some(gitlab::hook(&hook_url, &secret)),
                )
                .await
                .ok()
                .filter(|r| (200..300).contains(&r.status))
                .and_then(|r| r.body["id"].as_i64());
            match made {
                Some(hook) => {
                    let mut tx = self.store.begin().await?;
                    tx.set_hook(added.repo.id, Some(hook)).await?;
                    tx.commit().await?;
                    added.repo.hook_id = Some(hook);
                }
                None => {
                    added.manual_hook = Some(ManualHook {
                        url: hook_url,
                        secret,
                    })
                }
            }
        }
        Ok(added)
    }

    /// Changes a repo's description and default branch (`None`: unchanged).
    pub async fn update_repo(
        &self,
        id: RepoId,
        description: Option<&str>,
        default_branch: Option<&str>,
        actor: &str,
    ) -> Result<Repo> {
        let mut tx = self.store.begin().await?;
        let mut repo = tx.get_repo(id).await?;
        if let Some(description) = description {
            repo.description = description.trim().to_owned();
        }
        if let Some(branch) = default_branch {
            repo.default_branch = branch.to_owned();
        }
        tx.update_repo(id, &repo.description, &repo.default_branch)
            .await?;
        tx.append_audit(
            actor,
            AuditAction::WorkspaceChange,
            &repo.url,
            "edited repo",
        )
        .await?;
        tx.commit().await?;
        Ok(repo)
    }

    /// Deletes `repo` from `workspace`'s list (`NotFound` when it is another
    /// workspace's) and queues the deletion of the GitLab hook Sun made for it.
    pub async fn remove_workspace_repo(
        &self,
        workspace: WorkspaceId,
        repo: RepoId,
        actor: &str,
    ) -> Result<()> {
        let mut tx = self.store.begin().await?;
        let name = tx.get_workspace(workspace).await?.name;
        let row = tx.get_repo(repo).await?;
        if row.workspace_id != workspace {
            return Err(Error::NotFound(format!("repo {repo}")));
        }
        let hook_queued = queue_hook_deletion(&mut tx, &row).await?;
        tx.delete_repo(repo).await?;
        let detail = format!("removed repo {}", row.url);
        tx.append_audit(actor, AuditAction::WorkspaceChange, &name, &detail)
            .await?;
        tx.commit().await?;
        if hook_queued {
            self.outbox_wake.notify_one();
        }
        Ok(())
    }
}

/// `InvalidInput` unless `url` is a clone URL git can take safely: an
/// `http`, `https`, `ssh` or `git` URL with a host, or the scp-like
/// `[user@]host:path`; never an option
/// (`-…`), a remote helper (`::`) or `https://user:token@host/…` (the token
/// would land in the row, the audit log and every context pack).
fn check_repo_url(url: &str) -> Result<()> {
    let bad = |why: &str| Err(Error::InvalidInput(format!("`{url}` {why}")));
    if url.is_empty()
        || url.len() > 2048
        || url.starts_with('-')
        || url.contains("::")
        || url.chars().any(|c| c.is_whitespace() || c.is_control())
    {
        return bad("is not a repository URL.");
    }
    match url.split_once("://") {
        Some((scheme, rest)) => {
            if !matches!(scheme, "http" | "https" | "ssh" | "git") {
                return bad("is not a repository URL (http, https, ssh or git).");
            }
            let authority = rest.split('/').next().unwrap_or_default();
            if authority.is_empty() {
                return bad("has no host.");
            }
            if scheme != "ssh" && authority.contains('@') {
                return Err(Error::InvalidInput(
                    "Leave credentials out of the URL: the connection provides them.".into(),
                ));
            }
        }
        None => {
            // `[user@]host:path`: a non-empty host before the first `:`, a
            // non-empty path after it, and `@` only in the host part.
            let Some((authority, path)) = url.split_once(':') else {
                return bad(
                    "is not a repository URL (https://host/owner/name or git@host:owner/name).",
                );
            };
            let host = authority.rsplit('@').next().unwrap_or_default();
            if host.is_empty() || path.is_empty() || path.contains('@') {
                return bad("is not a repository URL (git@host:owner/name).");
            }
        }
    }
    Ok(())
}

/// Outbox target of a GitLab hook deletion, delivered by the kernel itself
/// ([`delete_hook`]) rather than a mod.
pub(crate) const DELETE_HOOK: &str = "vcs.delete_hook";

/// A [`DELETE_HOOK`] row's payload: the hook and how to reach it. Secret
/// *names* only; the token is read when the row is delivered.
#[derive(Debug, Serialize, serde::Deserialize)]
struct HookDeletion {
    connection_id: VcsConnectionId,
    workspace_id: WorkspaceId,
    instance_url: String,
    token_secret: Option<String>,
    refresh_secret: Option<String>,
    refresh_redirect_uri: Option<String>,
    webhook_secret: Option<String>,
    /// `group/name`.
    project: String,
    hook_id: i64,
}

/// The outbox row deleting `hook` of the repo at `url`, keyed by the hook
/// (enqueueing it twice is a no-op).
fn hook_row(connection: &VcsConnection, url: &str, hook: i64) -> Result<OutboxItem> {
    let payload = HookDeletion {
        connection_id: connection.id,
        workspace_id: connection.workspace_id,
        instance_url: connection.instance_url.clone(),
        token_secret: connection.token_secret.clone(),
        refresh_secret: connection.refresh_secret.clone(),
        refresh_redirect_uri: connection.oauth_redirect_uri.clone(),
        webhook_secret: connection.webhook_secret.clone(),
        project: repo_slug(url),
        hook_id: hook,
    };
    Ok(OutboxItem {
        id: OutboxId::generate(),
        run_id: None,
        idem_key: IdemKey::outside_run("vcs_hook", &format!("{}:{hook}", connection.id)),
        target: DELETE_HOOK.into(),
        payload: serde_json::to_value(payload).map_err(|e| Error::Protocol(e.to_string()))?,
        status: OutboxStatus::Pending,
        result: None,
    })
}

/// Queues, in `tx`, the deletion of the hook Sun made for `repo` (if any);
/// true when one was queued. The workspace's GitLab connection on the
/// repo's host must still exist (a disconnect queues its own).
async fn queue_hook_deletion(tx: &mut Tx, repo: &Repo) -> Result<bool> {
    let Some(hook) = repo.hook_id else {
        return Ok(false);
    };
    let connection = connection_of_repo(tx, repo.workspace_id, repo).await?;
    let Some(connection) = connection.filter(|c| c.provider == VcsProvider::Gitlab) else {
        return Ok(false);
    };
    tx.enqueue(&hook_row(&connection, &repo.url, hook)?).await?;
    tx.set_hook(repo.id, None).await?;
    Ok(true)
}

/// Delivers a [`DELETE_HOOK`] row: `DELETE /projects/:id/hooks/:hook_id`.
/// Gone already (404) counts as deleted. A disconnected connection's
/// secrets are deleted with its last hook.
pub(crate) async fn delete_hook(store: &Store, vcs: &Vcs, item: &OutboxItem) -> Result<Value> {
    let hook: HookDeletion = serde_json::from_value(item.payload.clone())
        .map_err(|e| Error::InvalidInput(format!("{DELETE_HOOK} payload: {e}")))?;
    let live = match store.read().await?.get_connection(hook.connection_id).await {
        Ok(connection) => Some(connection),
        Err(e) => match Error::from(e) {
            Error::NotFound(_) => None,
            e => return Err(e),
        },
    };
    // Disconnected: the row carries what reaching GitLab still needs.
    let connection = live.clone().unwrap_or_else(|| VcsConnection {
        id: hook.connection_id,
        workspace_id: hook.workspace_id,
        provider: VcsProvider::Gitlab,
        instance_url: hook.instance_url.clone(),
        account_login: String::new(),
        installation_id: None,
        token_secret: hook.token_secret.clone(),
        refresh_secret: hook.refresh_secret.clone(),
        token_expires_at: None,
        oauth_redirect_uri: hook.refresh_redirect_uri.clone(),
        webhook_secret: hook.webhook_secret.clone(),
        connected_by: String::new(),
        created_at: chrono::Utc::now(),
    });
    let path = format!(
        "{}/hooks/{}",
        gitlab::project_path(&hook.project),
        hook.hook_id
    );
    let reply = vcs.gitlab(&connection, "DELETE", &path, None).await?;
    if reply.status != 404 {
        reply.check("GitLab", "DELETE /projects/:id/hooks/:hook_id")?;
    }
    if live.is_none() {
        let mut tx = store.begin().await?;
        let open = [OutboxStatus::Pending, OutboxStatus::Unknown];
        let mut others = false;
        for status in open {
            others |= tx
                .outbox_with_status(status, u32::MAX)
                .await?
                .iter()
                .any(|row| {
                    row.id != item.id
                        && row.target == DELETE_HOOK
                        && row.payload["connection_id"] == json!(hook.connection_id)
                });
        }
        if !others {
            forget_secrets(&mut tx, &connection).await?;
        }
        tx.commit().await?;
    }
    Ok(json!({ "deleted": hook.hook_id }))
}

/// Deletes a connection's token, refresh token and webhook secret.
async fn forget_secrets(tx: &mut Tx, connection: &VcsConnection) -> Result<()> {
    let scope = SecretScope::Workspace(connection.workspace_id);
    for secret in [
        &connection.token_secret,
        &connection.refresh_secret,
        &connection.webhook_secret,
    ]
    .into_iter()
    .flatten()
    {
        tx.delete_secret(&scope, secret).await?;
    }
    Ok(())
}

/// `(url, description)` of every repo on `task`'s workspace, for its
/// context packs.
pub(crate) async fn workspace_repos(tx: &mut Tx, task: TaskId) -> Result<Vec<(String, String)>> {
    let workspace = workspace_of_task(tx, task).await?;
    Ok(tx
        .repos(workspace)
        .await?
        .into_iter()
        .map(|r| (r.url, r.description))
        .collect())
}

/// `workspace`'s connection to `provider` at `instance_url`, if any.
async fn existing(
    tx: &mut Tx,
    workspace: WorkspaceId,
    provider: VcsProvider,
    instance_url: &str,
) -> Result<Option<VcsConnection>> {
    Ok(tx
        .connections(workspace)
        .await?
        .into_iter()
        .find(|c| c.provider == provider && c.instance_url == instance_url))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use std::sync::Mutex as StdMutex;

    use sun_core::model::{Run, Task, Workflow};
    use sun_core::{RunId, RunStatus, TaskStatus, WorkflowId};
    use sun_mod_sdk::http::mock;
    use sun_store::{RunRepo, WorkflowRepo};

    use super::*;

    /// A task on Main bound to `repo_url`, and Main's GitLab connection to
    /// `instance` with an OAuth token that expired, refresh token `r-1`.
    async fn oauth_task(store: &Store, instance: &str, repo_url: &str) -> (TaskId, VcsConnection) {
        let scope = SecretScope::Workspace(WorkspaceId::MAIN);
        let id = VcsConnectionId::generate();
        let (token, refresh, webhook) = secret_names(id);
        let connection = VcsConnection {
            id,
            workspace_id: WorkspaceId::MAIN,
            provider: VcsProvider::Gitlab,
            instance_url: instance.into(),
            account_login: "ann".into(),
            installation_id: None,
            token_secret: Some(token.clone()),
            refresh_secret: Some(refresh.clone()),
            token_expires_at: Some(chrono::Utc::now() - chrono::TimeDelta::minutes(1)),
            oauth_redirect_uri: Some("https://sun.test/api/v1/vcs/gitlab/callback".into()),
            webhook_secret: Some(webhook),
            connected_by: "user:ann".into(),
            created_at: chrono::Utc::now(),
        };
        let repo = crate::gates::tests::repo(repo_url);
        let workflow = Workflow {
            id: WorkflowId::generate(),
            workspace_id: WorkspaceId::MAIN,
            name: "w".into(),
        };
        let task = Task {
            workflow_id: Some(workflow.id),
            status: TaskStatus::Backlog,
            ..Task::new(WorkspaceId::MAIN, "t")
        };
        let run = Run {
            id: RunId::generate(),
            task_id: task.id,
            workflow_id: workflow.id,
            workflow_version: 1,
            status: RunStatus::Queued,
            end_reason: None,
            budget: Default::default(),
            repo_ids: vec![repo.id],
        };
        let mut tx = store.begin().await.unwrap();
        tx.put_connection(&connection).await.unwrap();
        tx.set_secret(&scope, &token, "old-access-token")
            .await
            .unwrap();
        tx.set_secret(&scope, &refresh, "r-1-refresh")
            .await
            .unwrap();
        tx.insert_repo(&repo).await.unwrap();
        tx.insert_workflow(&workflow).await.unwrap();
        tx.save_workflow_version(workflow.id, "name: w\nsteps: []\n", "h")
            .await
            .unwrap();
        tx.insert_task(&task).await.unwrap();
        tx.insert_run(&run).await.unwrap();
        let eco = crate::ecosystem_scope().unwrap();
        tx.set_secret(&eco, gitlab::OAUTH_URL, instance)
            .await
            .unwrap();
        tx.set_secret(&eco, gitlab::OAUTH_CLIENT_ID, "client")
            .await
            .unwrap();
        tx.set_secret(&eco, gitlab::OAUTH_CLIENT_SECRET, "client-secret")
            .await
            .unwrap();
        tx.commit().await.unwrap();
        (task.id, connection)
    }

    /// A git host mod: records each call and answers with the next queued
    /// error, then `ok`.
    struct Mod {
        calls: StdMutex<Vec<ModCall>>,
        fail: StdMutex<Vec<Error>>,
    }

    /// What the kernel makes of the SDK's error for an HTTP 401
    /// (`data.http_status`, `sun-mod`'s `mod_error`).
    fn unauthorized() -> Error {
        Error::Refused {
            http_status: 401,
            message: "mod gitlab: POST /projects/:id/merge_requests: HTTP 401 Unauthorized".into(),
        }
    }

    #[async_trait::async_trait]
    impl ModLink for Mod {
        async fn call(&self, call: ModCall) -> Result<Value> {
            self.calls.lock().unwrap().push(call);
            match self.fail.lock().unwrap().pop() {
                Some(error) => Err(error),
                None => Ok(json!({ "ok": true })),
            }
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn vcs_mod_calls_get_the_workspace_token_refreshed_and_retried_on_401() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).await.unwrap();
        let refreshes = Arc::new(StdMutex::new(Vec::new()));
        let seen = refreshes.clone();
        let (gitlab, _) = mock("", move |r| match r.target.as_str() {
            "/oauth/token" => {
                let mut seen = seen.lock().unwrap();
                seen.push(r.text.clone());
                let n = seen.len();
                (
                    200,
                    json!({ "access_token": format!("access-token-{n}"),
                              "refresh_token": format!("refresh-token-{n}"), "expires_in": 7200 }),
                )
            }
            _ => (404, json!({})),
        })
        .unwrap();
        let (task, connection) =
            oauth_task(&store, &gitlab, &format!("{gitlab}/acme/app.git")).await;
        let vcs = Vcs::new(store.clone());
        let mods = Mod {
            calls: StdMutex::default(),
            fail: StdMutex::default(),
        };
        let call = |mod_name: &str| ModCall {
            mod_name: mod_name.into(),
            method: "open_pr".into(),
            params: json!({ "repo": "acme/app" }),
            idem_key: Some(IdemKey::outside_run("test", "k")),
        };
        let token_of = |i: usize| mods.calls.lock().unwrap()[i].params["credentials"].clone();

        // Expired: refreshed first, with the redirect URI, as a form.
        call_git_host(&store, &vcs, &mods, Some(task), call("gitlab"))
            .await
            .unwrap();
        assert_eq!(
            token_of(0),
            json!({ "token": "access-token-1", "api_url": format!("{gitlab}/api/v4") })
        );
        let form = refreshes.lock().unwrap()[0].clone();
        assert!(form.contains("grant_type=refresh_token"), "{form}");
        assert!(form.contains("refresh_token=r-1-refresh"), "{form}");
        assert!(
            form.contains(
                "redirect_uri=https%3A%2F%2Fsun.test%2Fapi%2Fv1%2Fvcs%2Fgitlab%2Fcallback"
            ),
            "{form}"
        );
        // The rotated refresh token and the expiry were stored together.
        let scope = SecretScope::Workspace(WorkspaceId::MAIN);
        let refresh = connection.refresh_secret.as_deref().unwrap();
        let mut tx = store.read().await.unwrap();
        assert_eq!(
            tx.secret_value(&scope, refresh).await.unwrap(),
            "refresh-token-1"
        );
        let stored = tx.get_connection(connection.id).await.unwrap();
        assert!(stored.token_expires_at.unwrap() > chrono::Utc::now());
        drop(tx);

        // Fresh now: no refresh. A 401 from the host forces one refresh and
        // one retry with the new token, same idem key.
        *mods.fail.lock().unwrap() = vec![unauthorized()];
        let reply = call_git_host(&store, &vcs, &mods, Some(task), call("gitlab")).await;
        assert_eq!(reply.unwrap(), json!({ "ok": true }));
        assert_eq!(token_of(1)["token"], "access-token-1");
        assert_eq!(token_of(2)["token"], "access-token-2");
        assert_eq!(
            mods.calls.lock().unwrap()[2].idem_key,
            call("gitlab").idem_key
        );
        assert_eq!(refreshes.lock().unwrap().len(), 2);
        // Retried once only.
        *mods.fail.lock().unwrap() = vec![unauthorized(), unauthorized()];
        assert!(
            call_git_host(&store, &vcs, &mods, Some(task), call("gitlab"))
                .await
                .is_err()
        );
        assert_eq!(mods.calls.lock().unwrap().len(), 5);
        // A 401 only mentioned in the text (no status as data): no retry,
        // no refresh.
        let text_only = Error::InvalidInput(
            "mod gitlab: POST /projects/:id/notes: HTTP 401 Unauthorized".into(),
        );
        *mods.fail.lock().unwrap() = vec![text_only];
        assert!(
            call_git_host(&store, &vcs, &mods, Some(task), call("gitlab"))
                .await
                .is_err()
        );
        assert_eq!(mods.calls.lock().unwrap().len(), 6);
        assert_eq!(refreshes.lock().unwrap().len(), 3);

        // Not a git host mod, a mod of another host, or no task: as sent
        // (the mod's own env token applies).
        for (mod_name, task) in [
            ("slack", Some(task)),
            ("github", Some(task)),
            ("gitlab", None),
        ] {
            call_git_host(&store, &vcs, &mods, task, call(mod_name))
                .await
                .unwrap();
            let last = mods.calls.lock().unwrap().last().unwrap().clone();
            assert_eq!(last.params, json!({ "repo": "acme/app" }), "{mod_name}");
        }
    }

    #[test]
    fn vcs_urls_are_checked_and_hosts_parsed() {
        assert_eq!(
            check_url("https://gitlab.acme.io/").unwrap(),
            "https://gitlab.acme.io"
        );
        assert!(
            check_url("http://127.0.0.1:9/x").is_ok(),
            "loopback in debug"
        );
        for bad in [
            "http://gitlab.acme.io",
            "ftp://x",
            "https://user:pw@gitlab.acme.io",
            "https://gitlab.acme.io?x=1",
            "https://",
            "https://a b",
        ] {
            assert!(check_url(bad).is_err(), "{bad}");
        }
        for (url, host) in [
            ("https://github.com/acme/web.git", "github.com"),
            ("https://GitLab.acme.io:8443/g/s/n", "gitlab.acme.io:8443"),
            ("git@github.com:acme/web.git", "github.com"),
            ("ssh://git@gitlab.com:22/g/n", "gitlab.com"),
        ] {
            assert_eq!(host_of(url).as_deref(), Some(host), "{url}");
        }
        assert_eq!(encode("a/b c"), "a%2Fb%20c");
        // Repo URLs git accepts, plus git safety.
        for ok in [
            "https://github.com/acme/web.git",
            "http://git.local/repo",
            "ssh://git@gitlab.com:22/g/n",
            "git://host/x",
            "git@github.com:acme/web.git",
            "host:path",
        ] {
            assert!(check_repo_url(ok).is_ok(), "{ok}");
        }
        for bad in [
            "",
            "-uupload-pack=x",
            "ext::sh -c id",
            "ftp://host/x",
            "https:///x",
            "https://user:token@github.com/acme/web.git",
            "github.com/acme/web",
            ":path",
            "host:",
            "host:user@path",
            "git@host:a b",
        ] {
            assert!(check_repo_url(bad).is_err(), "{bad}");
        }
    }
}

#[cfg(test)]
mod deployment_secrets {
    use super::{github, gitlab};
    use sun_core::model::Secret;

    /// The secrets the kernel reads from the ecosystem scope are the ones
    /// `Secret::is_deployment` keeps there.
    #[test]
    fn deployment_secrets_the_kernel_reads_stay_in_the_ecosystem() {
        let names = [
            crate::users::SMTP_PASSWORD,
            crate::knowledge::EMBEDDING_API_KEY,
            github::APP_ID,
            github::APP_SLUG,
            github::APP_PRIVATE_KEY,
            github::APP_WEBHOOK_SECRET,
            github::APP_API_URL,
            gitlab::OAUTH_URL,
            gitlab::OAUTH_CLIENT_ID,
            gitlab::OAUTH_CLIENT_SECRET,
        ];
        for name in names {
            assert!(Secret::is_deployment(name), "{name}");
        }
    }
}
