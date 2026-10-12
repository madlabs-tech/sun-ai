//! GitLab **Adapter**: OAuth (authorize, code exchange, refresh), the user,
//! the project picker and project hooks. The OAuth app is ecosystem-wide;
//! a self-hosted GitLab without one uses an access token.

use serde_json::{Value, json};
use sun_core::{Error, Result, Timestamp};
use sun_store::Store;

use super::{Body, RemoteRepo, check_url, ecosystem_secret, encode, send};

/// Ecosystem secrets of the OAuth app.
pub const OAUTH_URL: &str = "GITLAB_OAUTH_URL";
/// The OAuth app's Application ID.
pub const OAUTH_CLIENT_ID: &str = "GITLAB_OAUTH_CLIENT_ID";
/// The OAuth app's secret.
pub const OAUTH_CLIENT_SECRET: &str = "GITLAB_OAUTH_CLIENT_SECRET";
/// What GitLab sign-in needs (`OAuth::load`); the URL defaults to gitlab.com.
pub(crate) const OAUTH_REQUIRED: &[&str] = &[OAUTH_CLIENT_ID, OAUTH_CLIENT_SECRET];
const DEFAULT_URL: &str = "https://gitlab.com";
/// Picker page size.
const PER_PAGE: u32 = 50;

/// The OAuth app, loaded per call.
pub(crate) struct OAuth {
    /// The GitLab it signs in to; the connection's `instance_url`.
    pub(crate) url: String,
    client_id: String,
    client_secret: String,
}

/// Tokens from a code exchange or a refresh.
pub(crate) struct Grant {
    pub(crate) access_token: String,
    pub(crate) refresh_token: String,
    pub(crate) expires_at: Timestamp,
}

impl OAuth {
    /// `Unavailable` until an admin set the client id and secret.
    pub(crate) async fn load(store: &Store) -> Result<Self> {
        let (Some(client_id), Some(client_secret)) = (
            ecosystem_secret(store, OAUTH_CLIENT_ID).await?,
            ecosystem_secret(store, OAUTH_CLIENT_SECRET).await?,
        ) else {
            return Err(Error::Unavailable(format!(
                "GitLab sign-in isn't set up: an admin sets {OAUTH_CLIENT_ID} and \
                 {OAUTH_CLIENT_SECRET} in Ecosystem settings. Or connect with an access token."
            )));
        };
        let url = match ecosystem_secret(store, OAUTH_URL).await? {
            Some(url) => check_url(&url)?,
            None => DEFAULT_URL.into(),
        };
        Ok(Self {
            url,
            client_id: client_id.trim().to_owned(),
            client_secret: client_secret.trim().to_owned(),
        })
    }

    /// Where the browser goes to approve Sun (scopes `api read_user`).
    pub(crate) fn authorize_url(&self, redirect_uri: &str, state: &str) -> String {
        format!(
            "{}/oauth/authorize?client_id={}&redirect_uri={}&response_type=code&state={}&scope={}",
            self.url,
            encode(&self.client_id),
            encode(redirect_uri),
            encode(state),
            encode("api read_user")
        )
    }

    /// Trades the callback's `code` for tokens.
    pub(crate) async fn exchange(&self, code: &str, redirect_uri: &str) -> Result<Grant> {
        self.token(vec![
            ("grant_type", "authorization_code".into()),
            ("code", code.into()),
            ("redirect_uri", redirect_uri.into()),
        ])
        .await
    }

    /// New tokens for `refresh_token` (GitLab rotates the refresh token
    /// too), with the `redirect_uri` the code was issued for, as GitLab's
    /// docs list it for this grant.
    pub(crate) async fn refresh(
        &self,
        refresh_token: &str,
        redirect_uri: Option<&str>,
    ) -> Result<Grant> {
        let mut form = vec![
            ("grant_type", "refresh_token".into()),
            ("refresh_token", refresh_token.into()),
        ];
        if let Some(uri) = redirect_uri {
            form.push(("redirect_uri", uri.into()));
        }
        self.token(form).await
    }

    async fn token(&self, mut form: Vec<(&'static str, String)>) -> Result<Grant> {
        form.push(("client_id", self.client_id.clone()));
        form.push(("client_secret", self.client_secret.clone()));
        let url = format!("{}/oauth/token", self.url);
        let reply = send("POST", url, None, Some(Body::Form(form))).await?;
        if reply.status == 400 || reply.status == 401 {
            return Err(Error::InvalidInput(
                "GitLab refused the sign-in: start again from Connect GitLab.".into(),
            ));
        }
        let body = reply.check("GitLab", "POST /oauth/token")?;
        let (Some(access), Some(refresh)) = (
            body["access_token"].as_str(),
            body["refresh_token"].as_str(),
        ) else {
            return Err(Error::Protocol("GitLab sent no tokens".into()));
        };
        sun_core::redact::remember_secret(access);
        sun_core::redact::remember_secret(refresh);
        // About 2 h; refreshed a little early (`super::REFRESH_EARLY`).
        let lifetime = body["expires_in"].as_i64().unwrap_or(7200);
        Ok(Grant {
            access_token: access.to_owned(),
            refresh_token: refresh.to_owned(),
            expires_at: chrono::Utc::now() + chrono::Duration::seconds(lifetime),
        })
    }
}

/// `/api/v4` of an instance.
pub(crate) fn api(instance_url: &str) -> String {
    format!("{instance_url}/api/v4")
}

/// The token's username; `InvalidInput` when GitLab refuses the token.
pub(crate) async fn username(instance_url: &str, token: &str) -> Result<String> {
    let reply = send(
        "GET",
        format!("{}/user", api(instance_url)),
        Some(token.into()),
        None,
    )
    .await?;
    if matches!(reply.status, 401 | 403) {
        return Err(Error::InvalidInput(
            "GitLab refused the token: give it the `api` and `read_user` scopes.".into(),
        ));
    }
    let body = reply.check("GitLab", "GET /user")?;
    body["username"]
        .as_str()
        .map(str::to_owned)
        .ok_or_else(|| Error::Protocol("GitLab sent no username".into()))
}

fn remote(project: &Value) -> Option<RemoteRepo> {
    Some(RemoteRepo {
        name: project["path_with_namespace"].as_str()?.to_owned(),
        url: project["http_url_to_repo"].as_str()?.to_owned(),
        web_url: project["web_url"].as_str().unwrap_or_default().to_owned(),
        default_branch: project["default_branch"]
            .as_str()
            .unwrap_or("main")
            .to_owned(),
        description: project["description"]
            .as_str()
            .unwrap_or_default()
            .to_owned(),
        private: project["visibility"].as_str() != Some("public"),
    })
}

/// `GET /projects?membership=true…`: one page (from 1) of the user's projects.
pub(crate) fn projects_path(query: &str, page: u32) -> String {
    let mut path = format!(
        "/projects?membership=true&simple=true&order_by=last_activity_at&per_page={PER_PAGE}&page={page}"
    );
    if !query.is_empty() {
        path.push_str(&format!("&search={}", encode(query)));
    }
    path
}

/// The picker's rows from a `GET /projects` body.
pub(crate) fn projects(body: &Value) -> Vec<RemoteRepo> {
    body.as_array()
        .map(|all| all.iter().filter_map(remote).collect())
        .unwrap_or_default()
}

/// The repo from a `GET /projects/:path` body.
pub(crate) fn project(body: &Value) -> Result<RemoteRepo> {
    remote(body).ok_or_else(|| Error::Protocol("GitLab sent an incomplete project".into()))
}

/// `/projects/<group%2Fname>`.
pub(crate) fn project_path(slug: &str) -> String {
    format!("/projects/{}", encode(slug))
}

/// A project hook's settings: the events Sun's triggers read
/// (issues, comments, merge requests, pipelines), signed with `secret`.
pub(crate) fn hook(url: &str, secret: &str) -> Value {
    json!({
        "url": url,
        "token": secret,
        "issues_events": true,
        "note_events": true,
        "merge_requests_events": true,
        "pipeline_events": true,
        "push_events": false,
        "enable_ssl_verification": url.starts_with("https://"),
    })
}
