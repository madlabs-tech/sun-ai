//! Sun Cloud billing, proxied (**Proxy** over the cloud's HTTP API): each
//! workspace's subscription (`/subscriptions/...`) and support tickets
//! (`/support`). Every call carries the
//! kernel's cloud token, the acting member (`X-User-ID`) and an
//! `X-Request-ID`; bodies are rebuilt from allowlists, and writes that
//! charge send an `Idempotency-Key` derived from the client's
//! `idempotency_key`, the member and the path. Sun Cloud asks back with
//! `GET /internal/workspaces/{id}/seat_count` (its own token).
//!
//! - Workspace reads (summary, prices): any member there. Writes (checkout,
//!   portal, seats): its owners and admins. Support: deployment owners.
//!   The kernel token (a machine) gets 403 on all of it.
//! - The cloud's 404 (or any failure to answer) is `503 billing_unavailable`,
//!   never the Free plan; 402, 409 and 429 pass through with their code.
// ponytail: wallets (`/billing/*`) are deferred with credits; add them here
// when sun.madlabs.tech sells credits.

use std::time::Duration;

use axum::extract::{Path, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Extension, Json, Router};
use oauth2::reqwest;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sun_core::model::Role;
use sun_core::{MemberId, WorkspaceId};

use crate::Api;
use crate::auth::Caller;
use crate::error::{ApiError, ApiResult};
use crate::routes::{Body, id};

/// A cloud answer is a page of rows at most; anything larger is refused.
const MAX_ANSWER: usize = 1024 * 1024;
const MAX_KEY: usize = 200;
const MAX_SEATS: u32 = 10_000;
const MAX_SUBJECT: usize = 200;
const MAX_MESSAGE: usize = 5000;

/// The client of Sun Cloud's billing API, with this kernel's cloud token.
/// Never logs the token.
pub struct CloudBilling {
    url: String,
    token: String,
    http: reqwest::Client,
}

impl CloudBilling {
    /// Billing at Sun Cloud's `url`, authenticated with `token`.
    pub fn new(url: &str, token: &str) -> sun_core::Result<Self> {
        let http = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(15))
            .build()
            .map_err(|e| sun_core::Error::Unavailable(format!("http client: {e}")))?;
        Ok(Self {
            url: url.trim_end_matches('/').to_owned(),
            token: token.to_owned(),
            http,
        })
    }

    /// `method path` for `user`, with `body` as JSON and `idempotency` as
    /// the `Idempotency-Key`. The cloud's JSON comes back with its status.
    async fn send(
        &self,
        method: reqwest::Method,
        path: &str,
        user: MemberId,
        body: Option<Value>,
        idempotency: Option<&str>,
    ) -> ApiResult<Response> {
        let mut request = self
            .http
            .request(method, format!("{}{path}", self.url))
            .bearer_auth(&self.token)
            .header("x-user-id", user.to_string())
            .header("x-request-id", ulid::Ulid::generate().to_string());
        if let Some(key) = idempotency {
            request = request.header("idempotency-key", key);
        }
        if let Some(body) = body {
            request = request
                .header(header::CONTENT_TYPE, "application/json")
                .body(body.to_string());
        }
        let response = request.send().await.map_err(|e| {
            tracing::warn!("Sun Cloud {path}: {}", e.without_url());
            unavailable()
        })?;
        let status = response.status().as_u16();
        let retry_after = response
            .headers()
            .get(header::RETRY_AFTER)
            .and_then(|v| v.to_str().ok())
            .filter(|v| !v.is_empty() && v.len() <= 10 && v.bytes().all(|b| b.is_ascii_digit()))
            .map(str::to_owned);
        if response
            .content_length()
            .is_some_and(|n| n > MAX_ANSWER as u64)
        {
            return Err(unavailable());
        }
        let bytes = response.bytes().await.map_err(|_| unavailable())?;
        if bytes.len() > MAX_ANSWER {
            return Err(unavailable());
        }
        let answer: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
        if (200..300).contains(&status) {
            let status = StatusCode::from_u16(status).unwrap_or(StatusCode::OK);
            return Ok((status, Json(answer)).into_response());
        }
        if !matches!(status, 402 | 403 | 409 | 429 | 400 | 422) {
            tracing::warn!("Sun Cloud {path}: answered {status}");
        }
        let mut refused = refusal(status, &answer).into_response();
        if let Some(seconds) = retry_after.filter(|_| status == 429)
            && let Ok(value) = HeaderValue::from_str(&seconds)
        {
            refused.headers_mut().insert(header::RETRY_AFTER, value);
        }
        Ok(refused)
    }
}

/// Sun Cloud didn't answer, or doesn't know what was asked: never "Free".
fn unavailable() -> ApiError {
    ApiError::new(
        StatusCode::SERVICE_UNAVAILABLE,
        "billing_unavailable",
        "Billing is unavailable right now: Sun Cloud didn't answer. Try again in a few minutes.",
    )
}

/// A cloud error (`{error, code}`) as the person can act on it: known codes
/// get Sun's own copy; a 403 or 422 keeps the cloud's message (it says why);
/// anything else (404, the cloud refusing our token, 5xx) is [`unavailable`].
fn refusal(status: u16, answer: &Value) -> ApiError {
    let code = answer["code"].as_str().or(answer["error"]["code"].as_str());
    let message = answer["message"]
        .as_str()
        .or(answer["error"]["message"].as_str())
        .or(answer["error"].as_str());
    let (status, code, copy) = match (status, code) {
        (_, Some("idempotency_conflict")) => (
            StatusCode::CONFLICT,
            "idempotency_conflict",
            "This was already sent with other details. Start again.",
        ),
        (_, Some("seat_capacity_overcommitted")) => (
            StatusCode::CONFLICT,
            "seat_capacity_overcommitted",
            "The workspace has more people than seats. Buy seats or remove someone first.",
        ),
        (_, Some("stale_seat_preview")) => (
            StatusCode::CONFLICT,
            "stale_seat_preview",
            "The seats or their price changed since the preview. Preview again.",
        ),
        (_, Some("provider_unavailable")) => (
            StatusCode::SERVICE_UNAVAILABLE,
            "provider_unavailable",
            "The payment provider isn't answering. Try again in a few minutes.",
        ),
        (_, Some("kernel_unavailable")) => (
            StatusCode::SERVICE_UNAVAILABLE,
            "kernel_unavailable",
            "Sun Cloud couldn't count this workspace's members. Try again in a few minutes.",
        ),
        (402, _) => (
            StatusCode::PAYMENT_REQUIRED,
            "insufficient_balance",
            "Payment is required first. Check the plan's payment method, then try again.",
        ),
        (429, _) => (
            StatusCode::TOO_MANY_REQUESTS,
            "rate_limited",
            "Too many billing requests. Wait a moment, then try again.",
        ),
        // `user_required` would be our bug (no X-User-ID): unavailable, below.
        (403, c) if c != Some("user_required") => {
            let copy = message.unwrap_or("Sun Cloud refused this.");
            return ApiError::new(StatusCode::FORBIDDEN, "forbidden", copy);
        }
        (400 | 422, c) if c != Some("user_required") => {
            let copy = message.unwrap_or("Sun Cloud refused the request.");
            return ApiError::invalid(copy);
        }
        _ => return unavailable(),
    };
    ApiError::new(status, code, copy)
}

fn cloud(api: &Api) -> ApiResult<&CloudBilling> {
    api.cloud.as_deref().ok_or_else(|| {
        ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "billing_unavailable",
            "Billing needs Sun Cloud: this kernel isn't connected to it.",
        )
    })
}

fn forbidden(message: &str) -> ApiError {
    ApiError::new(StatusCode::FORBIDDEN, "forbidden", message)
}

/// The signed-in person paying; the kernel token is a machine.
fn person(caller: &Caller) -> ApiResult<MemberId> {
    caller.member_id.ok_or_else(|| {
        forbidden("Billing is for people: sign in with your own account, not the kernel token.")
    })
}

/// The workspace in the path, which exists (the guard already hid the ones
/// the caller isn't a member of).
async fn workspace(api: &Api, text: &str) -> ApiResult<WorkspaceId> {
    let workspace = id(text, "workspace")?;
    api.engine.workspace(workspace).await?;
    Ok(workspace)
}

/// Its owners and admins (and deployment owners) change a workspace's plan.
fn may_pay(caller: &Caller, workspace: WorkspaceId) -> ApiResult<()> {
    let admin = caller
        .role_in(workspace)
        .is_some_and(|r| r.covers(Role::Admin));
    match caller.is_owner || admin {
        true => Ok(()),
        false => Err(forbidden(
            "Only a workspace owner or admin can change its plan or seats.",
        )),
    }
}

/// The `Idempotency-Key` for a client's `idempotency_key`: scoped to the
/// person and the path, so one person's key never replays another's.
fn idempotency(user: MemberId, path: &str, key: &str) -> ApiResult<String> {
    let valid = !key.is_empty()
        && key.chars().count() <= MAX_KEY
        && key.bytes().all(|b| b.is_ascii_graphic());
    if !valid {
        return Err(ApiError::invalid(format!(
            "`idempotency_key` must be 1–{MAX_KEY} visible ASCII characters."
        )));
    }
    Ok(crate::auth::digest(&format!("{user}:{path}:{key}")))
}

fn seats(n: u32, field: &str) -> ApiResult<u32> {
    match (1..=MAX_SEATS).contains(&n) {
        true => Ok(n),
        false => Err(ApiError::invalid(format!(
            "`{field}` must be 1–{MAX_SEATS}."
        ))),
    }
}

/// A token the cloud made (a purchase version): short and URL-safe.
fn token_like(text: &str, field: &str) -> ApiResult<()> {
    let valid = !text.is_empty()
        && text.len() <= MAX_KEY
        && text
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_.=".contains(&b));
    match valid {
        true => Ok(()),
        false => Err(ApiError::invalid(format!(
            "`{field}` must be 1–{MAX_KEY} letters, digits, - _ . or =."
        ))),
    }
}

/// The routes any signed-in person reaches; each handler checks the rest.
pub(crate) fn routes() -> Router<Api> {
    Router::new()
        .route("/subscriptions/{id}/summary", get(summary))
        .route("/subscriptions/{id}/prices", get(prices))
        .route("/subscriptions/checkout-sessions", post(checkout))
        .route("/subscriptions/{id}/portal-sessions", post(portal))
        .route(
            "/subscriptions/{id}/seats/purchase-preview",
            post(seat_preview),
        )
        .route("/subscriptions/{id}/seats/purchases", post(seat_purchase))
        .route("/subscriptions/{id}/seats/reconcile", post(reconcile))
        .route("/subscriptions/{id}/cancel", post(cancel))
}

/// A read of workspace `text`'s subscription by a member there.
async fn read_workspace(api: &Api, caller: &Caller, text: &str, what: &str) -> ApiResult<Response> {
    let cloud = cloud(api)?;
    let user = person(caller)?;
    let workspace = workspace(api, text).await?;
    let path = format!("/api/v1/subscriptions/{workspace}/{what}");
    cloud
        .send(reqwest::Method::GET, &path, user, None, None)
        .await
}

async fn summary(
    State(api): State<Api>,
    Extension(caller): Extension<Caller>,
    Path(text): Path<String>,
) -> ApiResult<Response> {
    read_workspace(&api, &caller, &text, "summary").await
}

async fn prices(
    State(api): State<Api>,
    Extension(caller): Extension<Caller>,
    Path(text): Path<String>,
) -> ApiResult<Response> {
    read_workspace(&api, &caller, &text, "prices").await
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct NewCheckout {
    workspace_id: String,
    plan: String,
    interval: String,
    seats: u32,
    idempotency_key: String,
}

/// `POST /subscriptions/checkout-sessions`: `{checkout_url, session_id}`;
/// the payer is the caller, an owner or admin of the workspace.
async fn checkout(
    State(api): State<Api>,
    Extension(caller): Extension<Caller>,
    Body(input): Body<NewCheckout>,
) -> ApiResult<Response> {
    let cloud = cloud(&api)?;
    let user = person(&caller)?;
    let workspace = workspace(&api, &input.workspace_id).await?;
    may_pay(&caller, workspace)?;
    if !matches!(input.plan.as_str(), "pro" | "business") {
        return Err(ApiError::invalid("`plan` must be `pro` or `business`."));
    }
    if !matches!(input.interval.as_str(), "month" | "year") {
        return Err(ApiError::invalid("`interval` must be `month` or `year`."));
    }
    let seats = seats(input.seats, "seats")?;
    let path = "/api/v1/subscriptions/checkout-sessions";
    let key = idempotency(user, path, &input.idempotency_key)?;
    let body = json!({
        "workspace_id": workspace,
        "plan": input.plan,
        "interval": input.interval,
        "seats": seats,
        "idempotency_key": key,
        "customer_email": caller.email(),
    });
    cloud
        .send(reqwest::Method::POST, path, user, Some(body), Some(&key))
        .await
}

/// A write on workspace `text`'s subscription by an owner or admin there.
async fn write_workspace(
    api: &Api,
    caller: &Caller,
    text: &str,
    what: &str,
    body: Option<Value>,
    idempotency_key: Option<&str>,
) -> ApiResult<Response> {
    let cloud = cloud(api)?;
    let user = person(caller)?;
    let workspace = workspace(api, text).await?;
    may_pay(caller, workspace)?;
    let path = format!("/api/v1/subscriptions/{workspace}/{what}");
    let key = idempotency_key
        .map(|k| idempotency(user, &path, k))
        .transpose()?;
    let body = match (body, &key) {
        (Some(mut body), Some(key)) => {
            body["idempotency_key"] = json!(key);
            Some(body)
        }
        (body, _) => body,
    };
    cloud
        .send(reqwest::Method::POST, &path, user, body, key.as_deref())
        .await
}

/// `POST /subscriptions/{id}/portal-sessions`: `{url}`.
async fn portal(
    State(api): State<Api>,
    Extension(caller): Extension<Caller>,
    Path(text): Path<String>,
) -> ApiResult<Response> {
    write_workspace(&api, &caller, &text, "portal-sessions", None, None).await
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SeatPreview {
    additional_seats: u32,
}

/// `POST /subscriptions/{id}/seats/purchase-preview`: what the seats cost now.
async fn seat_preview(
    State(api): State<Api>,
    Extension(caller): Extension<Caller>,
    Path(text): Path<String>,
    Body(input): Body<SeatPreview>,
) -> ApiResult<Response> {
    let additional_seats = seats(input.additional_seats, "additional_seats")?;
    let body = json!({ "additional_seats": additional_seats });
    write_workspace(
        &api,
        &caller,
        &text,
        "seats/purchase-preview",
        Some(body),
        None,
    )
    .await
}

/// The preview's version, a number or the cloud's own token.
#[derive(Deserialize, Serialize)]
#[serde(untagged)]
enum Version {
    Number(u64),
    Text(String),
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SeatPurchase {
    additional_seats: u32,
    expected_current_seats: u32,
    expected_purchase_version: Version,
    accepted_amount_minor: u64,
    currency: String,
    idempotency_key: String,
}

/// `POST /subscriptions/{id}/seats/purchases`: buys the previewed seats at
/// the accepted amount.
async fn seat_purchase(
    State(api): State<Api>,
    Extension(caller): Extension<Caller>,
    Path(text): Path<String>,
    Body(input): Body<SeatPurchase>,
) -> ApiResult<Response> {
    let additional_seats = seats(input.additional_seats, "additional_seats")?;
    if let Version::Text(version) = &input.expected_purchase_version {
        token_like(version, "expected_purchase_version")?;
    }
    let currency = input.currency.to_ascii_lowercase();
    if currency.len() != 3 || !currency.bytes().all(|b| b.is_ascii_lowercase()) {
        return Err(ApiError::invalid(
            "`currency` must be a three-letter code, e.g. `idr`.",
        ));
    }
    let body = json!({
        "additional_seats": additional_seats,
        "expected_current_seats": input.expected_current_seats,
        "expected_purchase_version": input.expected_purchase_version,
        "accepted_amount_minor": input.accepted_amount_minor,
        "currency": currency,
    });
    let key = Some(input.idempotency_key.as_str());
    write_workspace(&api, &caller, &text, "seats/purchases", Some(body), key).await
}

/// `POST /subscriptions/{id}/seats/reconcile`: the cloud recounts members
/// (it asks [`seat_count`]).
async fn reconcile(
    State(api): State<Api>,
    Extension(caller): Extension<Caller>,
    Path(text): Path<String>,
) -> ApiResult<Response> {
    write_workspace(&api, &caller, &text, "seats/reconcile", None, None).await
}

/// `POST /subscriptions/{id}/cancel`: `{status, period_end}`; the plan runs
/// to the end of the period it was paid for.
async fn cancel(
    State(api): State<Api>,
    Extension(caller): Extension<Caller>,
    Path(text): Path<String>,
) -> ApiResult<Response> {
    write_workspace(&api, &caller, &text, "cancel", None, None).await
}

/// A call for the signed-in person, not about one workspace.
async fn as_person(
    api: &Api,
    caller: &Caller,
    method: reqwest::Method,
    path: &str,
    body: Option<Value>,
) -> ApiResult<Response> {
    let cloud = cloud(api)?;
    let user = person(caller)?;
    cloud.send(method, path, user, body, None).await
}

/// `GET /support` (deployment owners): the tickets they opened at Sun.
pub(crate) async fn tickets(
    State(api): State<Api>,
    Extension(caller): Extension<Caller>,
) -> ApiResult<Response> {
    as_person(
        &api,
        &caller,
        reqwest::Method::GET,
        "/account/support",
        None,
    )
    .await
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct NewTicket {
    subject: String,
    body: String,
}

/// `POST /support` `{subject, body}` (deployment owners): 201 the ticket.
pub(crate) async fn open_ticket(
    State(api): State<Api>,
    Extension(caller): Extension<Caller>,
    Body(input): Body<NewTicket>,
) -> ApiResult<Response> {
    let subject = input.subject.trim();
    if subject.is_empty()
        || subject.chars().count() > MAX_SUBJECT
        || subject.contains(char::is_control)
    {
        return Err(ApiError::invalid(format!(
            "The subject must be 1–{MAX_SUBJECT} characters on one line."
        )));
    }
    let message = input.body.trim();
    if message.is_empty() || message.chars().count() > MAX_MESSAGE {
        return Err(ApiError::invalid(format!(
            "The message must be 1–{MAX_MESSAGE} characters."
        )));
    }
    let body = json!({ "subject": subject, "body": message });
    let method = reqwest::Method::POST;
    as_person(&api, &caller, method, "/account/support", Some(body)).await
}

/// `GET /internal/workspaces/{id}/seat_count` (Sun Cloud, with its token;
/// no session): `{seat_count}`, the workspace's members.
pub(crate) async fn seat_count(
    State(api): State<Api>,
    headers: HeaderMap,
    Path(text): Path<String>,
) -> ApiResult<Json<Value>> {
    let presented = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "));
    let trusted = match (&api.cloud, presented) {
        (Some(cloud), Some(token)) => crate::auth::same(token.as_bytes(), cloud.token.as_bytes()),
        _ => false,
    };
    if !trusted {
        return Err(ApiError::new(
            StatusCode::UNAUTHORIZED,
            "unauthorized",
            "Only Sun Cloud asks this, with its token.",
        ));
    }
    let workspace = workspace(&api, &text).await?;
    let count = api.engine.workspace_members(workspace).await?.len();
    Ok(Json(json!({ "seat_count": count })))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    #[test]
    fn idempotency_keys_are_bounded_and_scoped() {
        let (ann, bob) = (MemberId::generate(), MemberId::generate());
        let key = idempotency(ann, "/p", "k-1").unwrap();
        assert_eq!(key.len(), 64);
        assert_eq!(key, idempotency(ann, "/p", "k-1").unwrap(), "stable");
        assert_ne!(key, idempotency(bob, "/p", "k-1").unwrap());
        assert_ne!(key, idempotency(ann, "/q", "k-1").unwrap());
        assert!(idempotency(ann, "/p", &"k".repeat(MAX_KEY)).is_ok());
        for bad in ["", "has space", "tab\t", &"k".repeat(MAX_KEY + 1)] {
            assert!(idempotency(ann, "/p", bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn cloud_errors_map_to_actions_never_free() {
        let status = |code: u16, body: Value| refusal(code, &body).into_response().status();
        assert_eq!(status(404, json!({})), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(status(401, json!({})), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(status(500, json!({})), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(
            status(402, json!({ "code": "insufficient_balance" })),
            StatusCode::PAYMENT_REQUIRED
        );
        assert_eq!(
            status(409, json!({ "error": "x", "code": "stale_seat_preview" })),
            StatusCode::CONFLICT
        );
        assert_eq!(
            status(503, json!({ "code": "provider_unavailable" })),
            StatusCode::SERVICE_UNAVAILABLE
        );
        assert_eq!(
            status(403, json!({ "error": "Pro only" })),
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            status(403, json!({ "code": "user_required" })),
            StatusCode::SERVICE_UNAVAILABLE
        );
        assert_eq!(status(429, json!({})), StatusCode::TOO_MANY_REQUESTS);
    }
}
