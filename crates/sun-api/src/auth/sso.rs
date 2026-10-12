//! Sign-in through the providers admins manage in Settings → Sign-in,
//! one **Strategy** per [`SsoKind`]:
//! - `google` and `oidc`: the OIDC authorization code flow with PKCE and a
//!   nonce, checking the ID token (signature, audience, nonce, `at_hash`).
//!   Discovery runs on every login, so a provider's key rotation or an
//!   admin's edit applies at once.
//! - `github`: GitHub's OAuth2 code flow with PKCE, then `GET /user` (the
//!   subject is the numeric user id, the issuer `github`) and `GET
//!   /user/emails` (the primary verified email).
//!
//! `GET /api/v1/auth/sso/{id}/login` → provider →
//! `GET /api/v1/auth/sso/{id}/callback` → the member linked to this identity
//! is found, or (invite only) the invited member whose email the provider
//! verified is linked to it; the login is audited, a session starts and the
//! browser lands on `/`, or on the `return_to` hash the login was started
//! with (a notification's deep link, e.g. `#/approve/<run>/<step>`). The
//! `state` is single use, lasts 10 minutes and is bound to the browser by a
//! cookie. A failed callback lands on `/#/login/failed`
//! (`/#/login/denied` for a disallowed email domain or a deactivated member,
//! `/#/login/invite` for anyone nobody invited). SSO sign-ins skip Sun's
//! 2-step check: the provider owns MFA for those people.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use axum::extract::{Path, Query, State};
use axum::http::header::{ACCEPT, LOCATION, SET_COOKIE, USER_AGENT};
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::{Extension, Json};
use oauth2::basic::BasicClient;
use openidconnect::core::{CoreAuthenticationFlow, CoreClient, CoreProviderMetadata};
use openidconnect::{
    AccessTokenHash, AuthType, AuthUrl, AuthorizationCode, ClientId, ClientSecret, CsrfToken,
    EndpointMaybeSet, EndpointNotSet, EndpointSet, IssuerUrl, Nonce, OAuth2TokenResponse,
    PkceCodeChallenge, PkceCodeVerifier, RedirectUrl, Scope, TokenResponse, TokenUrl, reqwest,
};
use serde::Deserialize;
use serde_json::{Value, json};
use sun_core::Error;
use sun_core::sso::{GITHUB_ISSUER, GOOGLE_ISSUER, SsoKind, SsoProvider};

use super::{COOKIE_NAME, Caller, SESSION_TTL, cookie, digest, same};
use crate::Api;
use crate::error::{ApiError, ApiResult};
use crate::hosts::random_hex;
use crate::routes::Body;

const STATE_COOKIE: &str = "sun_sso_state";
const COOKIE_PATH: &str = "/api/v1/auth/sso";
/// Time from clicking "Continue with …" to coming back from the provider.
const LOGIN_TTL: Duration = Duration::from_secs(10 * 60);
/// Logins in flight at once; the login route is open, so this is bounded.
const MAX_PENDING: usize = 1024;
/// Longest subject or email accepted from a provider.
const MAX_CLAIM: usize = 256;

/// Where the Google and GitHub presets live; tests point them at a mock
/// (`Api::with_preset_urls`).
#[derive(Debug, Clone)]
pub struct PresetUrls {
    /// Google's issuer, [`GOOGLE_ISSUER`].
    pub google: String,
    /// GitHub's authorize and token endpoints' base, `https://github.com`.
    pub github_web: String,
    /// GitHub's REST API base, `https://api.github.com`.
    pub github_api: String,
}

impl Default for PresetUrls {
    fn default() -> Self {
        Self {
            google: GOOGLE_ISSUER.into(),
            github_web: "https://github.com".into(),
            github_api: "https://api.github.com".into(),
        }
    }
}

impl PresetUrls {
    /// The OIDC issuer to discover for `provider`; `None` for GitHub.
    fn issuer<'a>(&'a self, provider: &'a SsoProvider) -> Option<&'a str> {
        match provider.kind {
            SsoKind::Google => Some(&self.google),
            SsoKind::Github => None,
            SsoKind::Oidc => provider.issuer_url.as_deref(),
        }
    }
}

type OidcClient = CoreClient<
    EndpointSet,
    EndpointNotSet,
    EndpointNotSet,
    EndpointNotSet,
    EndpointMaybeSet,
    EndpointMaybeSet,
>;
type GithubClient =
    BasicClient<EndpointSet, EndpointNotSet, EndpointNotSet, EndpointNotSet, EndpointSet>;

/// Logins in flight, and where the presets live.
#[derive(Default)]
pub(crate) struct SsoState {
    pending: Mutex<HashMap<String, Pending>>,
    pub(crate) urls: PresetUrls,
}

struct Pending {
    /// The provider id the login started with.
    provider: String,
    verifier: PkceCodeVerifier,
    /// OIDC only.
    nonce: Option<Nonce>,
    expires: Instant,
    /// The UI hash to land on, already checked by [`return_to`].
    return_to: Option<String>,
}

/// Who the provider says signed in.
struct Identity {
    issuer: String,
    subject: String,
    /// Only an email the provider verified.
    email: Option<String>,
}

/// A safe UI hash to come back to: `#/` plus route characters only, so the
/// redirect can never leave this origin.
fn return_to(hash: Option<String>) -> Option<String> {
    hash.filter(|h| {
        h.len() <= 200
            && h.starts_with("#/")
            && h[2..]
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '/' | '_' | '-' | '.'))
    })
}

impl SsoState {
    fn remember(&self, state: String, pending: Pending) -> ApiResult<()> {
        let mut map = self.pending.lock().unwrap_or_else(|e| e.into_inner());
        let now = Instant::now();
        map.retain(|_, p| p.expires > now);
        if map.len() >= MAX_PENDING {
            return Err(
                Error::Unavailable("too many sign-ins in progress; try again".into()).into(),
            );
        }
        map.insert(state, pending);
        Ok(())
    }

    /// Takes the pending login for `state` (single use), if not expired.
    fn take(&self, state: &str) -> Option<Pending> {
        let mut map = self.pending.lock().unwrap_or_else(|e| e.into_inner());
        map.remove(state).filter(|p| p.expires > Instant::now())
    }
}

fn http() -> sun_core::Result<reqwest::Client> {
    reqwest::Client::builder()
        // Following redirects would open the kernel to SSRF.
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(10))
        .build()
        .map_err(|e| Error::Unavailable(format!("http client: {e}")))
}

/// The OIDC client of `provider` at `issuer`, after discovery.
async fn oidc_client(
    http: &reqwest::Client,
    issuer: &str,
    provider: &SsoProvider,
    secret: String,
    redirect: RedirectUrl,
) -> ApiResult<OidcClient> {
    let issuer = IssuerUrl::new(issuer.to_owned())
        .map_err(|e| Error::InvalidInput(format!("issuer_url: {e}")))?;
    let metadata = CoreProviderMetadata::discover_async(issuer, http)
        .await
        .map_err(|e| Error::Unavailable(format!("OIDC discovery failed: {e}")))?;
    Ok(CoreClient::from_provider_metadata(
        metadata,
        ClientId::new(provider.client_id.clone()),
        Some(ClientSecret::new(secret)),
    )
    .set_redirect_uri(redirect))
}

/// GitHub's OAuth2 client for `provider`.
fn github_client(
    urls: &PresetUrls,
    provider: &SsoProvider,
    secret: String,
    redirect: RedirectUrl,
) -> ApiResult<GithubClient> {
    let url = |e: openidconnect::url::ParseError| Error::InvalidInput(format!("github url: {e}"));
    let auth = AuthUrl::new(format!("{}/login/oauth/authorize", urls.github_web)).map_err(url)?;
    let token =
        TokenUrl::new(format!("{}/login/oauth/access_token", urls.github_web)).map_err(url)?;
    Ok(BasicClient::new(ClientId::new(provider.client_id.clone()))
        .set_auth_uri(auth)
        .set_token_uri(token)
        .set_redirect_uri(redirect)
        .set_client_secret(ClientSecret::new(secret))
        // GitHub reads the client secret from the form body.
        .set_auth_type(AuthType::RequestBody))
}

/// Provider `id` and its client secret; 404 when there is no such provider
/// or it has no secret (it isn't offered on the sign-in page either).
async fn provider(api: &Api, id: &str) -> ApiResult<(SsoProvider, String)> {
    match api.engine.sso_provider(id).await? {
        Some((provider, Some(secret))) => Ok((provider, secret)),
        _ => Err(ApiError::new(
            StatusCode::NOT_FOUND,
            "not_found",
            "That sign-in provider is not set up on this ecosystem.",
        )),
    }
}

/// `<public url>/api/v1/auth/sso/{id}/callback`: register it at the provider.
fn callback_url(api: &Api, id: &str) -> String {
    format!(
        "{}/api/v1/auth/sso/{id}/callback",
        api.config.public_url.trim_end_matches('/')
    )
}

fn redirect_uri(api: &Api, id: &str) -> ApiResult<RedirectUrl> {
    RedirectUrl::new(callback_url(api, id))
        .map_err(|e| Error::InvalidInput(format!("public_url: {e}")).into())
}

/// `; Secure` when the kernel is reached over https.
pub(crate) fn secure(api: &Api) -> &'static str {
    if api.config.public_url.starts_with("https://") {
        "; Secure"
    } else {
        ""
    }
}

#[derive(Deserialize)]
pub(crate) struct Login {
    return_to: Option<String>,
}

/// `GET /api/v1/auth/sso/{id}/login[?return_to=#/…]`: redirects to the provider.
pub(crate) async fn login(
    State(api): State<Api>,
    Path(id): Path<String>,
    Query(query): Query<Login>,
) -> ApiResult<Response> {
    let (provider, secret) = provider(&api, &id).await?;
    let callback = redirect_uri(&api, &provider.id)?;
    let (challenge, verifier) = PkceCodeChallenge::new_random_sha256();
    let (url, state, nonce) = match api.auth.sso.urls.issuer(&provider) {
        Some(issuer) => {
            let client = oidc_client(&http()?, issuer, &provider, secret, callback).await?;
            let (url, state, nonce) = client
                .authorize_url(
                    CoreAuthenticationFlow::AuthorizationCode,
                    CsrfToken::new_random,
                    Nonce::new_random,
                )
                .add_scope(Scope::new("email".into()))
                .add_scope(Scope::new("profile".into()))
                .set_pkce_challenge(challenge)
                .url();
            (url, state, Some(nonce))
        }
        None => {
            let client = github_client(&api.auth.sso.urls, &provider, secret, callback)?;
            let (url, state) = client
                .authorize_url(CsrfToken::new_random)
                .add_scope(Scope::new("read:user".into()))
                .add_scope(Scope::new("user:email".into()))
                .set_pkce_challenge(challenge)
                .url();
            (url, state, None)
        }
    };
    let pending = Pending {
        provider: provider.id,
        verifier,
        nonce,
        expires: Instant::now() + LOGIN_TTL,
        return_to: return_to(query.return_to),
    };
    api.auth.sso.remember(state.secret().clone(), pending)?;
    // `Lax`: the provider's redirect back is a cross-site navigation.
    let state_cookie = format!(
        "{STATE_COOKIE}={}; Path={COOKIE_PATH}; HttpOnly; SameSite=Lax; Max-Age={}{}",
        state.secret(),
        LOGIN_TTL.as_secs(),
        secure(&api)
    );
    Ok(redirect(url.as_str(), vec![state_cookie]))
}

#[derive(Deserialize)]
pub(crate) struct Callback {
    code: Option<String>,
    state: Option<String>,
}

/// `GET /api/v1/auth/sso/{id}/callback`: finishes the login and lands on the UI.
pub(crate) async fn callback(
    State(api): State<Api>,
    Path(id): Path<String>,
    headers: HeaderMap,
    Query(query): Query<Callback>,
) -> Response {
    let clear_state = format!("{STATE_COOKIE}=; Path={COOKIE_PATH}; HttpOnly; Max-Age=0");
    match finish(&api, &id, &headers, query).await {
        Ok((token, landing)) => {
            let session = format!(
                "{COOKIE_NAME}={token}; Path=/; HttpOnly; SameSite=Strict; Max-Age={}{}",
                SESSION_TTL.as_secs(),
                secure(&api)
            );
            let landing = format!("/{}", landing.unwrap_or_default());
            redirect(&landing, vec![clear_state, session])
        }
        Err(reason) => redirect(&format!("/#/login/{reason}"), vec![clear_state]),
    }
}

/// Why a callback failed, as shown to the login screen.
type Reason = &'static str;
const FAILED: Reason = "failed";

async fn finish(
    api: &Api,
    id: &str,
    headers: &HeaderMap,
    query: Callback,
) -> Result<(String, Option<String>), Reason> {
    let (Some(code), Some(state)) = (query.code, query.state) else {
        return Err(FAILED);
    };
    // The state must come back to the browser that started the login.
    let bound = cookie(headers, STATE_COOKIE).is_some_and(|c| same(c.as_bytes(), state.as_bytes()));
    if !bound {
        return Err(FAILED);
    }
    let pending = api.auth.sso.take(&state).ok_or(FAILED)?;
    if pending.provider != id {
        return Err(FAILED);
    }
    let (provider, secret) = provider(api, id).await.map_err(|_| FAILED)?;
    let redirect = redirect_uri(api, id).map_err(|_| FAILED)?;
    let code = AuthorizationCode::new(code);
    let http = http().map_err(|_| FAILED)?;
    let urls = &api.auth.sso.urls;
    let identity = match urls.issuer(&provider) {
        Some(issuer) => {
            let client = oidc_client(&http, issuer, &provider, secret, redirect)
                .await
                .map_err(|_| FAILED)?;
            let nonce = pending.nonce.as_ref().ok_or(FAILED)?;
            oidc_identity(&http, &client, code, pending.verifier, nonce).await?
        }
        None => {
            let client = github_client(urls, &provider, secret, redirect).map_err(|_| FAILED)?;
            let response = client
                .exchange_code(code)
                .set_pkce_verifier(pending.verifier)
                .request_async(&http)
                .await
                .map_err(|_| FAILED)?;
            github_identity(&http, urls, response.access_token().secret()).await?
        }
    };

    if !provider.allowed_domains.is_empty() {
        let domain = identity
            .email
            .as_deref()
            .and_then(|e| e.rsplit_once('@'))
            .map(|(_, d)| d.to_ascii_lowercase());
        if !domain.is_some_and(|d| provider.allowed_domains.contains(&d)) {
            return Err("denied");
        }
    }
    let bad = |v: &str| v.len() > MAX_CLAIM || v.chars().any(char::is_control);
    if identity.subject.is_empty()
        || bad(&identity.subject)
        || identity.email.as_deref().is_some_and(bad)
    {
        return Err(FAILED);
    }
    // Invite only: a member already linked to this identity, or the
    // invited member whose email the provider verified, is let in.
    let verified = identity
        .email
        .as_deref()
        .and_then(|e| sun_core::model::normalize_email(e).ok());
    let token = random_hex().map_err(|_| FAILED)?;
    let member = api
        .engine
        .sign_in_oidc(
            &identity.issuer,
            &identity.subject,
            verified.as_deref(),
            &digest(&token),
            chrono::Utc::now() + SESSION_TTL,
            &format!("signed in with sso:{id}"),
        )
        .await
        .map_err(|e| match e {
            Error::Forbidden(_) => "denied",
            _ => FAILED,
        })?;
    if member.is_none() {
        return Err("invite");
    }
    Ok((token, pending.return_to))
}

/// Exchanges `code`, checks the ID token and reads its identity.
async fn oidc_identity(
    http: &reqwest::Client,
    client: &OidcClient,
    code: AuthorizationCode,
    verifier: PkceCodeVerifier,
    nonce: &Nonce,
) -> Result<Identity, Reason> {
    let response = client
        .exchange_code(code)
        .map_err(|_| FAILED)?
        .set_pkce_verifier(verifier)
        .request_async(http)
        .await
        .map_err(|_| FAILED)?;
    let id_token = response.id_token().ok_or(FAILED)?;
    let verifier = client.id_token_verifier();
    let claims = id_token.claims(&verifier, nonce).map_err(|_| FAILED)?;
    if let Some(expected) = claims.access_token_hash() {
        let alg = id_token.signing_alg().map_err(|_| FAILED)?;
        let key = id_token.signing_key(&verifier).map_err(|_| FAILED)?;
        let actual =
            AccessTokenHash::from_token(response.access_token(), alg, key).map_err(|_| FAILED)?;
        if actual != *expected {
            return Err(FAILED);
        }
    }
    Ok(Identity {
        issuer: claims.issuer().as_str().to_owned(),
        subject: claims.subject().as_str().to_owned(),
        email: claims
            .email()
            .filter(|_| claims.email_verified() == Some(true))
            .map(|e| e.as_str().to_owned()),
    })
}

/// GitHub's `GET /user` (its fields are snake_case already).
#[derive(Deserialize)]
struct GithubUser {
    id: u64,
}

/// One entry of GitHub's `GET /user/emails`.
#[derive(Deserialize)]
struct GithubEmail {
    email: String,
    primary: bool,
    verified: bool,
}

/// The GitHub user behind `token`: their numeric id and primary verified email.
async fn github_identity(
    http: &reqwest::Client,
    urls: &PresetUrls,
    token: &str,
) -> Result<Identity, Reason> {
    let user: GithubUser = github_get(http, &format!("{}/user", urls.github_api), token).await?;
    let emails: Vec<GithubEmail> =
        github_get(http, &format!("{}/user/emails", urls.github_api), token).await?;
    Ok(Identity {
        issuer: GITHUB_ISSUER.into(),
        subject: user.id.to_string(),
        email: emails
            .into_iter()
            .find(|e| e.primary && e.verified)
            .map(|e| e.email),
    })
}

async fn github_get<T: serde::de::DeserializeOwned>(
    http: &reqwest::Client,
    url: &str,
    token: &str,
) -> Result<T, Reason> {
    let response = http
        .get(url)
        .bearer_auth(token)
        .header(ACCEPT, "application/vnd.github+json")
        .header(USER_AGENT, "sun")
        .send()
        .await
        .and_then(reqwest::Response::error_for_status)
        .map_err(|_| FAILED)?;
    let bytes = response.bytes().await.map_err(|_| FAILED)?;
    serde_json::from_slice(&bytes).map_err(|_| FAILED)
}

pub(crate) fn redirect(location: &str, cookies: Vec<String>) -> Response {
    let mut response = StatusCode::SEE_OTHER.into_response();
    let headers = response.headers_mut();
    if let Ok(value) = HeaderValue::from_str(location) {
        headers.insert(LOCATION, value);
    }
    for c in cookies {
        if let Ok(value) = HeaderValue::from_str(&c) {
            headers.append(SET_COOKIE, value);
        }
    }
    response
}

/// `GET /api/v1/auth`: what the login screen offers. Open. `providers`:
/// one "Continue with {name}" each, only those with a stored secret (any
/// other could only fail); `local`: members sign in with email and a
/// password or emailed code.
pub(crate) async fn providers(State(api): State<Api>) -> ApiResult<Json<Value>> {
    let providers: Vec<Value> = api
        .engine
        .sso_providers()
        .await?
        .into_iter()
        .filter(|(_, has_secret)| *has_secret)
        .map(|(p, _)| json!({ "id": p.id, "name": p.name, "kind": p.kind }))
        .collect();
    Ok(Json(json!({ "providers": providers, "local": true })))
}

/// `GET /api/v1/settings/sso` (`manage_settings`): every provider with
/// `has_secret` (the secret never comes back), and `redirect_url`, the
/// callback to register at a provider with `{id}` in place of its id.
pub(crate) async fn list(State(api): State<Api>) -> ApiResult<Json<Value>> {
    let providers: Vec<Value> = api
        .engine
        .sso_providers()
        .await?
        .into_iter()
        .map(|(p, has_secret)| {
            let mut view = json!(p);
            view["has_secret"] = json!(has_secret);
            view
        })
        .collect();
    Ok(Json(json!({
        "providers": providers,
        "redirect_url": callback_url(&api, "{id}"),
    })))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ProviderInput {
    kind: SsoKind,
    name: String,
    client_id: String,
    issuer_url: Option<String>,
    #[serde(default)]
    allowed_domains: Vec<String>,
    /// Absent or empty keeps the stored one; required for a new provider.
    client_secret: Option<String>,
}

/// `PUT /api/v1/settings/sso/{id} {kind, name, client_id, issuer_url?,
/// allowed_domains?, client_secret?}` (`manage_settings`): adds or replaces
/// the provider; the client secret is sealed as an ecosystem secret.
pub(crate) async fn put(
    State(api): State<Api>,
    Extension(caller): Extension<Caller>,
    Path(id): Path<String>,
    Body(input): Body<ProviderInput>,
) -> ApiResult<Json<Value>> {
    let secret = input.client_secret.filter(|s| !s.trim().is_empty());
    let exists = api.engine.sso_provider(&id).await?.is_some();
    if !exists && secret.is_none() {
        return Err(ApiError::invalid(
            "Enter the client secret from the provider.",
        ));
    }
    let provider = SsoProvider {
        id,
        kind: input.kind,
        name: input.name,
        client_id: input.client_id,
        issuer_url: input.issuer_url,
        allowed_domains: input.allowed_domains,
    };
    api.engine
        .put_sso_provider(provider, secret.as_deref().map(str::trim), &caller.actor())
        .await?;
    list(State(api)).await
}

/// `DELETE /api/v1/settings/sso/{id}` (`manage_settings`): removes the
/// provider and its secret. Members it linked keep their accounts.
pub(crate) async fn delete(
    State(api): State<Api>,
    Extension(caller): Extension<Caller>,
    Path(id): Path<String>,
) -> ApiResult<StatusCode> {
    api.engine.delete_sso_provider(&id, &caller.actor()).await?;
    Ok(StatusCode::NO_CONTENT)
}
