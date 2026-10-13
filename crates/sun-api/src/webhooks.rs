//! `POST /api/v1/webhooks/{mod}`: third-party deliveries forwarded to the
//! mod's `webhook` action. No token: the delivery proves itself.
//!
//! - A mod with a `WEBHOOK_SECRET` (mod-scoped secret) gets only deliveries
//!   the kernel verified with one of [`SCHEMES`] (GitHub, Gitea, GitLab,
//!   Jira, Trello), or that carry `?secret=<WEBHOOK_SECRET>` (Jira without
//!   signing). Git hosts get `{event, payload}`, Jira and Trello `{payload}`.
//! - A mod without one verifies deliveries itself (Slack's signing secret):
//!   it gets the raw `{headers, body}`, and its `FAILED` answer is a 401.
//!
//! A kernel-verified delivery with an id (`X-GitHub-Delivery`, also sent by
//! Gitea; `X-Gitlab-Event-UUID`) reaches the mod once: a replay of one that
//! succeeded, within [`REPLAY_WINDOW`], is answered 200 and dropped; one
//! that failed can be redelivered. One without an id, or for a
//! self-verifying mod, is not tracked.
//!
//! Slack wants an answer within 3 s, so a slow mod is answered `202` while
//! its call carries on. `HEAD` answers 200 (Trello probes the URL first).
//!
//! Git connections:
//! - `/webhooks/github` also takes the GitHub App's deliveries, signed with
//!   its `GITHUB_APP_WEBHOOK_SECRET` (ecosystem). After the signature and
//!   the replay guard, the kernel mirrors them first
//!   ([`sun_engine::Engine::github_delivery`]: `installation` removals,
//!   `pull_request`, `check_suite`, `check_run`, `status`), then forwards
//!   to the mod; a failed mirror is logged and never drops the forward.
//! - `POST /api/v1/webhooks/vcs/{connection_id}`: a GitLab project hook Sun
//!   made. `X-Gitlab-Token` must equal the connection's webhook secret
//!   (constant time; anything else is 401), then the kernel mirrors it
//!   ([`sun_engine::Engine::gitlab_delivery`]) and it goes to the `gitlab`
//!   mod's `webhook` action like any GitLab delivery.
//! - Bodies up to [`MAX_BODY`] (GitHub's own limit is 25 MB; a PR payload
//!   is well under 10 MB).

use std::collections::BTreeMap;
use std::time::Duration;

use axum::Json;
use axum::body::Bytes;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use base64::Engine as _;
use serde_json::{Value, json};
use sha1::Sha1;
use sha2::{Digest, Sha256};
use sun_core::model::VcsConnection;
use sun_core::{Error, ModCall, VcsConnectionId};

use crate::Api;
use crate::auth::same;
use crate::error::{ApiError, ApiResult};

/// Under Slack's 3 s budget, with room for the network.
const ANSWER_WITHIN: Duration = Duration::from_millis(2500);

/// How long the id of a delivery that succeeded is remembered. A failed
/// delivery's id is freed at once, so GitHub's Redeliver (same id) works.
const REPLAY_WINDOW: chrono::Duration = chrono::Duration::days(7);
/// Longest delivery id remembered; a longer one is not tracked.
const MAX_DELIVERY_ID: usize = 128;
/// Where a delivery names its id.
const DELIVERY_HEADERS: &[&str] = &["x-github-delivery", "x-gitlab-event-uuid"];
/// The largest delivery body taken (10 MiB).
pub(crate) const MAX_BODY: usize = 10 * 1024 * 1024;

/// A git host delivery the kernel mirrors (PRs, checks, installations)
/// before the mod sees it.
enum Mirror {
    /// The GitHub App's delivery.
    Github,
    /// A GitLab project hook on this connection.
    Gitlab(Box<VcsConnection>),
}

impl Mirror {
    /// Mirrors `{event, payload}`; an error is the kernel's to log, never
    /// the delivery's to fail.
    async fn apply(self, api: &Api, params: &Value) {
        let event = params["event"].as_str().unwrap_or_default();
        let payload = &params["payload"];
        let mirrored = match &self {
            Mirror::Github => api.engine.github_delivery(event, payload).await,
            Mirror::Gitlab(connection) => {
                api.engine.gitlab_delivery(connection, event, payload).await
            }
        };
        if let Err(error) = mirrored {
            eprintln!("sun: webhook {event}: mirror failed: {error}");
        }
    }
}

/// How a delivery proves it knows `WEBHOOK_SECRET`.
enum Proof {
    /// `prefix` + hex HMAC-SHA256 of the body.
    Sha256Hex { prefix: &'static str },
    /// The header value is the secret itself.
    Token,
    /// base64 HMAC-SHA1 of the body + the callback URL (the mod's
    /// `WEBHOOK_URL` secret, the URL the webhook was registered with).
    Sha1Base64WithUrl,
}

/// One provider's signature header (**Strategy** table).
struct Scheme {
    header: &'static str,
    proof: Proof,
    /// The mod takes `{event, payload}` (git hosts), else `{payload}`.
    event: bool,
}

/// First header present wins. GitHub also sends a legacy `X-Hub-Signature`
/// (SHA-1), so its `-256` header comes before Jira's; Gitea also sends
/// `X-Hub-Signature-256`, so its own header comes first.
const SCHEMES: &[Scheme] = &[
    Scheme {
        header: "x-gitea-signature",
        proof: Proof::Sha256Hex { prefix: "" },
        event: true,
    },
    Scheme {
        header: "x-hub-signature-256",
        proof: Proof::Sha256Hex { prefix: "sha256=" },
        event: true,
    },
    Scheme {
        header: "x-gitlab-token",
        proof: Proof::Token,
        event: true,
    },
    // Jira Cloud's webhook secret.
    Scheme {
        header: "x-hub-signature",
        proof: Proof::Sha256Hex { prefix: "sha256=" },
        event: false,
    },
    Scheme {
        header: "x-trello-webhook",
        proof: Proof::Sha1Base64WithUrl,
        event: false,
    },
];

/// Where git hosts name the event.
const EVENT_HEADERS: &[&str] = &[
    "x-github-event",
    "x-gitea-event",
    "x-forgejo-event",
    "x-gitlab-event",
];

pub(crate) async fn receive(
    State(api): State<Api>,
    Path(mod_name): Path<String>,
    Query(query): Query<BTreeMap<String, String>>,
    headers: HeaderMap,
    body: Bytes,
) -> ApiResult<Response> {
    let Some(mods) = api.mods.clone() else {
        return Err(ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "unavailable",
            "Mods are not running.",
        ));
    };
    let header = |name: &str| headers.get(name).and_then(|v| v.to_str().ok());
    let signed = SCHEMES
        .iter()
        .find_map(|scheme| Some((scheme, header(scheme.header)?)));
    // Any of them proves a delivery: the mod's own, and on `github` the App's.
    let mut secrets: Vec<String> = api
        .engine
        .mod_secret(&mod_name, "WEBHOOK_SECRET")
        .await?
        .into_iter()
        .collect();
    if mod_name == "github" {
        secrets.extend(api.engine.github_webhook_secret().await?);
    }
    let params = match (signed, query.get("secret"), secrets.is_empty()) {
        (Some((scheme, presented)), _, false) => {
            let mut proven = false;
            for secret in &secrets {
                let expected = match scheme.proof {
                    Proof::Sha256Hex { prefix } => {
                        format!(
                            "{prefix}{}",
                            hex(&hmac::<Sha256>(secret.as_bytes(), &[&body]))
                        )
                    }
                    Proof::Token => secret.clone(),
                    Proof::Sha1Base64WithUrl => {
                        let url = api.engine.mod_secret(&mod_name, "WEBHOOK_URL").await?;
                        let url = url.ok_or_else(unauthorized)?;
                        let mac = hmac::<Sha1>(secret.as_bytes(), &[&body, url.as_bytes()]);
                        base64::engine::general_purpose::STANDARD.encode(mac)
                    }
                };
                proven |= same(presented.as_bytes(), expected.as_bytes());
            }
            verified(proven)?;
            let event = scheme.event.then(|| {
                EVENT_HEADERS
                    .iter()
                    .find_map(|h| header(h))
                    .unwrap_or_default()
            });
            delivery(event, &body)?
        }
        (None, Some(token), false) => {
            let proven = secrets
                .iter()
                .fold(false, |ok, s| ok | same(token.as_bytes(), s.as_bytes()));
            verified(proven)?;
            delivery(None, &body)?
        }
        (None, None, true) => {
            let body = String::from_utf8(body.to_vec())
                .map_err(|_| ApiError::invalid("The webhook body is not UTF-8."))?;
            json!({ "headers": forwarded(&headers), "body": body })
        }
        // A secret is set but the delivery is unsigned, or the reverse.
        _ => return Err(unauthorized()),
    };
    // Only deliveries the kernel verified: anyone can post to a self-verifying
    // mod's URL, and must not use up a real delivery's id first.
    let replay_key = delivery_id(&headers)
        .filter(|_| !secrets.is_empty())
        .map(|id| format!("webhook:{mod_name}:{id}"));
    // Only a delivery the App's secret could have signed is mirrored.
    let mirror = (mod_name == "github" && !secrets.is_empty()).then_some(Mirror::Github);
    forward(&api, mods, mod_name, params, replay_key, mirror).await
}

/// `POST /api/v1/webhooks/vcs/{connection_id}`: a GitLab hook Sun made.
pub(crate) async fn receive_vcs(
    State(api): State<Api>,
    Path(connection): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> ApiResult<Response> {
    let header = |name: &str| headers.get(name).and_then(|v| v.to_str().ok());
    // An unknown or GitHub connection has no secret: 401 like a bad token.
    let secret = match connection.parse::<VcsConnectionId>() {
        Ok(id) => api.engine.vcs_hook_secret(id).await?,
        Err(_) => None,
    };
    let (Some(secret), Some(presented)) = (secret, header("x-gitlab-token")) else {
        return Err(unauthorized());
    };
    verified(same(presented.as_bytes(), secret.as_bytes()))?;
    let Some(mods) = api.mods.clone() else {
        return Err(ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "unavailable",
            "Mods are not running.",
        ));
    };
    let params = delivery(header("x-gitlab-event"), &body)?;
    let replay_key = delivery_id(&headers).map(|id| format!("webhook:vcs:{connection}:{id}"));
    // Verified above: the connection exists and is GitLab's.
    let mirror = match connection.parse::<VcsConnectionId>() {
        Ok(id) => Some(Mirror::Gitlab(Box::new(
            api.engine.vcs_connection(id).await?,
        ))),
        Err(_) => None,
    };
    forward(&api, mods, "gitlab".into(), params, replay_key, mirror).await
}

/// Mirrors a verified git host delivery, then hands it to the mod, once
/// per `replay_key`: a replay of a delivery that succeeded (2xx, or 202
/// while the mod carries on) is answered 200 and dropped. A failed one
/// frees its key, so the provider's Redeliver (same id) retries it.
async fn forward(
    api: &Api,
    mods: std::sync::Arc<dyn sun_core::ModLink>,
    mod_name: String,
    params: Value,
    replay_key: Option<String>,
    mirror: Option<Mirror>,
) -> ApiResult<Response> {
    if let Some(key) = &replay_key {
        let expires = chrono::Utc::now() + REPLAY_WINDOW;
        if !api.engine.use_once(key, expires).await? {
            return Ok(StatusCode::OK.into_response());
        }
    }
    if let Some(mirror) = mirror {
        mirror.apply(api, &params).await;
    }
    let answer = hand_over(api, mods, mod_name, params).await;
    if answer.is_err()
        && let Some(key) = &replay_key
    {
        // The delivery's own error is the answer; a failed release only
        // costs the Redeliver until the key expires.
        let _ = api.engine.release_once(key).await;
    }
    answer
}

/// Hands a verified delivery to the mod's `webhook` action.
async fn hand_over(
    api: &Api,
    mods: std::sync::Arc<dyn sun_core::ModLink>,
    mod_name: String,
    params: Value,
) -> ApiResult<Response> {
    crate::corrections::record_reverts(api, &params).await?;
    let call = ModCall {
        mod_name,
        method: "webhook".into(),
        params,
        idem_key: None,
    };
    let running = tokio::spawn(async move { mods.call(call).await });
    let reply = match tokio::time::timeout(ANSWER_WITHIN, running).await {
        Err(_) => return Ok(StatusCode::ACCEPTED.into_response()),
        Ok(joined) => joined.map_err(|_| Error::Unavailable("mod call panicked".into()))?,
    };
    match reply {
        Ok(reply) => Ok(match reply.get("response") {
            Some(response) if !response.is_null() => Json(response.clone()).into_response(),
            _ => StatusCode::OK.into_response(),
        }),
        // The mod refused the delivery (`FAILED`, e.g. a bad Slack signature).
        Err(Error::InvalidInput(_) | Error::Refused { .. }) => Err(unauthorized()),
        Err(e) => Err(e.into()),
    }
}

/// The delivery's id, when it sent one of [`DELIVERY_HEADERS`].
fn delivery_id(headers: &HeaderMap) -> Option<&str> {
    DELIVERY_HEADERS
        .iter()
        .find_map(|h| headers.get(*h)?.to_str().ok())
        .filter(|id| !id.is_empty() && id.len() <= MAX_DELIVERY_ID)
}

fn verified(ok: bool) -> ApiResult<()> {
    if ok { Ok(()) } else { Err(unauthorized()) }
}

fn unauthorized() -> ApiError {
    ApiError::new(
        StatusCode::UNAUTHORIZED,
        "unauthorized",
        "The webhook signature does not match.",
    )
}

/// `{event, payload}` (git hosts) or `{payload}` (Jira, Trello).
fn delivery(event: Option<&str>, body: &[u8]) -> ApiResult<Value> {
    let payload: Value = serde_json::from_slice(body)
        .map_err(|_| ApiError::invalid("The webhook body is not JSON."))?;
    Ok(match event {
        Some(event) => json!({ "event": event, "payload": payload }),
        None => json!({ "payload": payload }),
    })
}

/// Request headers for a self-verifying mod, minus the kernel's own credentials.
fn forwarded(headers: &HeaderMap) -> BTreeMap<String, String> {
    headers
        .iter()
        .filter(|(name, _)| !matches!(name.as_str(), "authorization" | "cookie"))
        .filter_map(|(name, value)| Some((name.as_str().to_owned(), value.to_str().ok()?.into())))
        .collect()
}

/// HMAC (RFC 2104) over the concatenated `parts`, for 64-byte-block hashes
/// (SHA-1, SHA-256).
pub(crate) fn hmac<D: Digest>(key: &[u8], parts: &[&[u8]]) -> Vec<u8> {
    let mut block = [0u8; 64];
    if key.len() > block.len() {
        let hashed = D::digest(key);
        block[..hashed.len()].copy_from_slice(&hashed);
    } else {
        block[..key.len()].copy_from_slice(key);
    }
    let pad = |byte: u8| block.map(|b| b ^ byte);
    let mut inner = D::new().chain_update(pad(0x36));
    for part in parts {
        inner.update(part);
    }
    D::new()
        .chain_update(pad(0x5c))
        .chain_update(inner.finalize())
        .finalize()
        .to_vec()
}

pub(crate) fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hmac_matches_rfc_test_vectors() {
        // RFC 4231 test case 2 (SHA-256), split in two parts.
        let mac = hmac::<Sha256>(b"Jefe", &[b"what do ya want ", b"for nothing?"]);
        assert_eq!(
            hex(&mac),
            "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
        );
        // RFC 2202 test case 2 (SHA-1).
        let mac = hmac::<Sha1>(b"Jefe", &[b"what do ya want for nothing?"]);
        assert_eq!(hex(&mac), "effcdf6ae5eb2fa2d27416d5f184df9c259a7c79");
        // A key longer than the block is hashed first (RFC 4231 case 6).
        let message = b"Test Using Larger Than Block-Size Key - Hash Key First";
        assert_eq!(
            hex(&hmac::<Sha256>(&[0xaa; 131], &[message])),
            "60e431591ee0b67f0d8a26aacbf5b77f8e0bc6213728c5140546040f0ee37f54"
        );
    }
}
