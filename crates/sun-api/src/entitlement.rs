//! Plans per workspace. [`CloudPolicy`] reads each workspace's policy from
//! Sun Cloud (`GET {url}/api/v1/policy/{workspace_id}`) and caches it;
//! `GET /entitlement` shows it, and [`within_plan`] refuses one more seat or
//! host past a cap. Without Sun Cloud every workspace is Community.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use axum::Json;
use axum::extract::{Query, State};
use axum::http::StatusCode;
use oauth2::reqwest;
use serde::Deserialize;
use serde_json::{Value, json};
use sun_core::WorkspaceId;
use sun_core::entitlement::{
    Entitlement, EntitlementProvider, EntitlementStatus, Limits, Plan, Quota,
};

use crate::Api;
use crate::error::{ApiError, ApiResult};
use crate::routes::id;

/// The longest a policy is trusted without asking again.
const MAX_TTL: Duration = Duration::from_secs(300);
/// How long the last good policy is served (as `stale`) while Sun Cloud fails.
const STALE_GRACE: Duration = Duration::from_secs(24 * 60 * 60);
/// A policy answer is a few hundred bytes; anything far larger is refused.
const MAX_POLICY_BYTES: u64 = 64 * 1024;
/// The policy schema this engine reads.
const SCHEMA_VERSION: u32 = 1;

/// `GET /api/v1/policy/{workspace_id}`'s answer.
#[derive(Deserialize)]
struct Policy {
    schema_version: u32,
    workspace_id: WorkspaceId,
    plan: Plan,
    paid: bool,
    #[serde(default)]
    limits: Limits,
    valid_for_seconds: u64,
}

/// Sun Cloud's per-workspace policy (**Proxy** with a TTL cache over the
/// cloud's policy endpoint). Fresh for the policy's `valid_for_seconds`
/// (at most 5 minutes); while refreshes fail the last good policy is served
/// as `stale` for 24 hours, then Community (`unavailable`). A workspace the
/// cloud doesn't know (404) is on Free.
// ponytail: concurrent misses each fetch; a per-workspace in-flight guard if
// the cloud ever sees the burst.
pub struct CloudPolicy {
    url: String,
    token: String,
    /// The last good entitlement per workspace and when it stops being fresh.
    cache: Mutex<HashMap<WorkspaceId, (Entitlement, Instant)>>,
    http: reqwest::Client,
    stale_grace: Duration,
}

impl CloudPolicy {
    /// The policy at Sun Cloud's `url`, authenticated with `token`.
    pub fn new(url: &str, token: &str) -> sun_core::Result<Self> {
        let http = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(5))
            .build()
            .map_err(|e| sun_core::Error::Unavailable(format!("http client: {e}")))?;
        Ok(Self {
            url: url.trim_end_matches('/').to_owned(),
            token: token.to_owned(),
            cache: Mutex::default(),
            http,
            stale_grace: STALE_GRACE,
        })
    }

    fn cached(&self, workspace: WorkspaceId) -> Option<(Entitlement, Instant)> {
        let cache = self.cache.lock().unwrap_or_else(|e| e.into_inner());
        cache.get(&workspace).cloned()
    }

    /// The policy now, with how long it stays fresh. Never logs the token.
    async fn fetch(&self, workspace: WorkspaceId) -> Result<(Entitlement, Duration), String> {
        let url = format!("{}/api/v1/policy/{workspace}", self.url);
        let response = self
            .http
            .get(url)
            .bearer_auth(&self.token)
            .send()
            .await
            .map_err(|e| format!("unreachable: {}", e.without_url()))?;
        let fresh = |plan, paid, limits, ttl: Duration| {
            let valid_until = chrono::Utc::now()
                + chrono::Duration::from_std(ttl).unwrap_or(chrono::Duration::zero());
            let status = EntitlementStatus::Cloud { valid_until };
            let entitlement = Entitlement {
                plan,
                paid,
                limits,
                status,
            };
            Ok((entitlement, ttl))
        };
        match response.status().as_u16() {
            200 => {}
            404 => return fresh(Plan::Free, false, Limits::default(), MAX_TTL),
            status => return Err(format!("answered {status}")),
        }
        if response
            .content_length()
            .is_some_and(|n| n > MAX_POLICY_BYTES)
        {
            return Err("policy too large".into());
        }
        let body = response
            .bytes()
            .await
            .map_err(|e| format!("reading: {}", e.without_url()))?;
        let policy: Policy =
            serde_json::from_slice(&body).map_err(|e| format!("bad policy: {e}"))?;
        if policy.schema_version != SCHEMA_VERSION {
            return Err(format!("policy schema {}", policy.schema_version));
        }
        if policy.workspace_id != workspace {
            return Err("policy for another workspace".into());
        }
        let ttl = Duration::from_secs(policy.valid_for_seconds).min(MAX_TTL);
        fresh(policy.plan, policy.paid, policy.limits, ttl)
    }
}

#[async_trait]
impl EntitlementProvider for CloudPolicy {
    async fn for_workspace(&self, workspace: WorkspaceId) -> Entitlement {
        let cached = self.cached(workspace);
        if let Some((entitlement, fresh_until)) = &cached
            && Instant::now() < *fresh_until
        {
            return entitlement.clone();
        }
        match self.fetch(workspace).await {
            Ok((entitlement, ttl)) => {
                let mut cache = self.cache.lock().unwrap_or_else(|e| e.into_inner());
                cache.insert(workspace, (entitlement.clone(), Instant::now() + ttl));
                entitlement
            }
            Err(why) => {
                tracing::warn!("Sun Cloud policy for workspace {workspace}: {why}");
                match cached {
                    Some((entitlement, fresh_until))
                        if Instant::now() < fresh_until + self.stale_grace =>
                    {
                        Entitlement {
                            status: EntitlementStatus::Stale,
                            ..entitlement
                        }
                    }
                    _ => Entitlement::community(EntitlementStatus::Unavailable),
                }
            }
        }
    }
}

#[derive(Deserialize)]
pub(crate) struct Named {
    workspace: Option<String>,
}

/// `GET /entitlement?workspace=<id>` (a member there; no workspace: Main):
/// `{plan, paid, status, limits, valid_until}`. `status` is `cloud`
/// (fresh until `valid_until`), `stale`, `unavailable` or `community`.
pub(crate) async fn get(
    State(api): State<Api>,
    Query(named): Query<Named>,
) -> ApiResult<Json<Value>> {
    let workspace = match named.workspace {
        Some(text) => id(&text, "workspace")?,
        None => WorkspaceId::MAIN,
    };
    api.engine.workspace(workspace).await?;
    let e = api.entitlements.for_workspace(workspace).await;
    Ok(Json(json!({
        "plan": e.plan,
        "paid": e.paid,
        "status": e.status.as_str(),
        "limits": e.limits,
        "valid_until": e.status.valid_until(),
    })))
}

/// Refuses one more `what` (a [`Quota`]) in `workspace` when `current`
/// already reach its plan's cap, with a message saying how to get unstuck.
/// No cap: always allowed (Community has none).
// ponytail: agents, runtimes and byo_keys carry caps but nothing enforces
// them yet; M4's per-workspace create handlers add one call each.
pub(crate) async fn within_plan(
    api: &Api,
    workspace: WorkspaceId,
    quota: Quota,
    what: &str,
    current: usize,
) -> ApiResult<()> {
    let entitlement = api.entitlements.for_workspace(workspace).await;
    match entitlement.limits.max(quota) {
        Some(max) if current >= usize::try_from(max).unwrap_or(usize::MAX) => Err(ApiError::new(
            StatusCode::FORBIDDEN,
            "plan_limit",
            format!(
                "Plan limit reached ({what}: {max} on the {} plan). Remove one or upgrade the plan.",
                entitlement.plan
            ),
        )),
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use axum::extract::Path;
    use axum::http::HeaderMap;
    use axum::response::IntoResponse;

    use super::*;

    /// A stand-in for Sun Cloud's policy endpoint: answers `reply` and counts hits.
    #[derive(Clone, Default)]
    struct Cloud {
        reply: Arc<Mutex<(u16, Value)>>,
        hits: Arc<AtomicUsize>,
    }

    impl Cloud {
        fn answer(&self, status: u16, body: Value) {
            *self.reply.lock().unwrap() = (status, body);
        }

        fn hits(&self) -> usize {
            self.hits.load(Ordering::SeqCst)
        }

        /// Serves on a free loopback port; the policy client of it.
        async fn start(&self) -> CloudPolicy {
            let cloud = self.clone();
            let app = axum::Router::new().route(
                "/api/v1/policy/{id}",
                axum::routing::get(move |Path(id): Path<String>, headers: HeaderMap| {
                    let cloud = cloud.clone();
                    async move {
                        cloud.hits.fetch_add(1, Ordering::SeqCst);
                        assert_eq!(headers["authorization"], "Bearer t0k");
                        let (status, mut body) = cloud.reply.lock().unwrap().clone();
                        if body.is_object() && body.get("workspace_id").is_none() {
                            body["workspace_id"] = json!(id);
                        }
                        (StatusCode::from_u16(status).unwrap(), Json(body)).into_response()
                    }
                }),
            );
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let url = format!("http://{}/", listener.local_addr().unwrap());
            tokio::spawn(async move { axum::serve(listener, app).await });
            CloudPolicy::new(&url, "t0k").unwrap()
        }
    }

    fn pro(valid_for_seconds: u64) -> Value {
        json!({ "schema_version": 1, "policy_revision": 7, "plan": "pro", "paid": true,
            "limits": { "seats": 3, "runtimes": null, "byo_keys": 2, "agents": 5, "hosts": 1 },
            "valid_for_seconds": valid_for_seconds })
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn cloud_policy_caches_refetches_and_fails_open() {
        let cloud = Cloud::default();
        cloud.answer(200, pro(1));
        let mut policy = cloud.start().await;
        let ops = WorkspaceId::generate();

        // 200: the policy, cached for its validity.
        let first = policy.for_workspace(ops).await;
        assert_eq!((first.plan, first.paid), (Plan::Pro, true));
        assert_eq!(first.limits.max(Quota::Hosts), Some(1));
        assert_eq!(first.limits.max(Quota::Runtimes), None);
        assert!(matches!(first.status, EntitlementStatus::Cloud { .. }));
        assert_eq!(policy.for_workspace(ops).await, first);
        assert_eq!(cloud.hits(), 1, "served from the cache");

        // Past its validity: fetched again.
        tokio::time::sleep(Duration::from_millis(1100)).await;
        policy.for_workspace(ops).await;
        assert_eq!(cloud.hits(), 2);

        // A failing cloud after a success: the old value, stale; past the
        // grace: Community, unavailable.
        tokio::time::sleep(Duration::from_millis(1100)).await;
        cloud.answer(500, json!({}));
        let stale = policy.for_workspace(ops).await;
        assert_eq!(stale.status, EntitlementStatus::Stale);
        assert_eq!((stale.plan, stale.limits), (first.plan, first.limits));
        policy.stale_grace = Duration::ZERO;
        assert_eq!(
            policy.for_workspace(ops).await,
            Entitlement::community(EntitlementStatus::Unavailable)
        );

        // 500 with nothing cached, or an answer that doesn't read: Community.
        let other = WorkspaceId::generate();
        assert_eq!(
            policy.for_workspace(other).await,
            Entitlement::community(EntitlementStatus::Unavailable)
        );
        for bad in [
            json!({ "schema_version": 2, "plan": "pro", "paid": true, "valid_for_seconds": 60 }),
            json!({ "schema_version": 1, "plan": "gold", "paid": true, "valid_for_seconds": 60 }),
            json!({ "schema_version": 1, "plan": "pro", "paid": true, "valid_for_seconds": 60,
                "workspace_id": WorkspaceId::generate() }),
        ] {
            cloud.answer(200, bad.clone());
            let e = policy.for_workspace(other).await;
            assert_eq!(e.status, EntitlementStatus::Unavailable, "{bad}");
        }

        // 404: a workspace the cloud doesn't know is on Free.
        cloud.answer(404, json!({ "error": "unknown workspace" }));
        let free = policy.for_workspace(other).await;
        assert_eq!((free.plan, free.paid), (Plan::Free, false));
        assert!(matches!(free.status, EntitlementStatus::Cloud { .. }));

        // A validity over five minutes is capped at five.
        cloud.answer(200, pro(86_400));
        let capped = WorkspaceId::generate();
        let EntitlementStatus::Cloud { valid_until } = policy.for_workspace(capped).await.status
        else {
            panic!("a cloud policy");
        };
        assert!(valid_until <= chrono::Utc::now() + chrono::Duration::seconds(301));
    }

    #[tokio::test]
    async fn an_unreachable_cloud_is_unavailable() {
        // Nothing listens on port 9 (discard) here.
        let policy = CloudPolicy::new("http://127.0.0.1:9", "t0k").unwrap();
        assert_eq!(
            policy.for_workspace(WorkspaceId::MAIN).await,
            Entitlement::community(EntitlementStatus::Unavailable)
        );
    }
}
