//! Marketplace runtime capacity, buyer side:
//! `/api/v1/marketplace/...` over the kernel's [`sun_core::Marketplace`]
//! (Sun Cloud). Leasing a task mints a join token bound to it, leases with
//! this kernel's public URL, and pins the task to a reset VM with a network
//! allowlist on the seller's host that joins with that token.
//!
//! - `GET /marketplace/listings`, `GET /marketplace/leases`: Sun Cloud's views.
//! - `POST /marketplace/leases` (admin) `{task_id, runtime, unit,
//!   max_price_points, model?, region?, accept_disclosure: true, secrets?}`
//!   → 201 lease. `secrets`: the ecosystem secrets allowed on the seller's
//!   machine, by name; the rest are withheld.
//! - `POST /marketplace/leases/{id}/accept` (admin): release the escrow.
//! - `POST /marketplace/leases/{id}/dispute` (admin) `{reason}`.
//! - `GET /marketplace/policy`, `PUT` (admin) `{allow: all|none|workflows,
//!   workflows, failover_max_price_points?}`: the ecosystem policy (local).
//!
//! Without a marketplace (outside Sun Cloud) every route but the policy answers 503.

use std::sync::Arc;

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use serde::Deserialize;
use serde_json::Value;
use sun_core::model::Repo;
use sun_core::{ExecEnvSpec, LeaseRequest, ListingFilter, Marketplace, TaskId, VmSpec};
use sun_engine::{MarketPolicy, TaskLease};

use crate::Api;
use crate::error::{ApiError, ApiResult};
use crate::routes::{Body, id};

const MAX_TEXT: usize = 200;
const MAX_REASON: usize = 2000;
const MAX_SECRETS: usize = 50;
/// Lima template when a listing names none (older Sun Cloud).
const DEFAULT_TEMPLATE: &str = "default";

/// The model API each runtime adapter calls from inside the task VM.
const API_HOSTS: &[(&str, &str)] = &[
    ("claude-code", "api.anthropic.com"),
    ("codex", "api.openai.com"),
    ("opencode", "api.anthropic.com"),
];

fn market(api: &Api) -> ApiResult<(Arc<dyn Marketplace>, String)> {
    match (&api.market, &api.public_url) {
        (Some(market), Some(url)) => Ok((market.clone(), url.clone())),
        _ => Err(ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "unavailable",
            "The marketplace needs Sun Cloud: this kernel isn't connected to it.",
        )),
    }
}

fn text(value: &str, what: &str) -> ApiResult<()> {
    let ok =
        !value.trim().is_empty() && value.len() <= MAX_TEXT && !value.chars().any(char::is_control);
    match ok {
        true => Ok(()),
        false => Err(ApiError::invalid(format!(
            "`{what}` must be 1–{MAX_TEXT} characters."
        ))),
    }
}

/// Sun Cloud ids go into its paths: `lea_<ulid>`-shaped only.
fn cloud_id<'a>(value: &'a str, what: &str) -> ApiResult<&'a str> {
    let ok = !value.is_empty()
        && value.len() <= 64
        && value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');
    ok.then_some(value)
        .ok_or_else(|| ApiError::invalid(format!("`{value}` is not a valid {what} id.")))
}

fn lease_id(value: &str) -> ApiResult<&str> {
    cloud_id(value, "lease")
}

/// `https://github.com/a/b.git`, `git@github.com:a/b.git`, `ssh://git@h/x` → the host.
fn remote_host(url: &str) -> Option<String> {
    let rest = url.split_once("://").map_or(url, |(_, rest)| rest);
    let rest = rest.split_once('@').map_or(rest, |(_, rest)| rest);
    let host = rest.split(['/', ':']).next()?;
    (!host.is_empty()).then(|| host.to_ascii_lowercase())
}

/// What the task VM may reach: the runtime's model API and the task's git remotes.
fn allowlist(runtime: &str, repos: &[Repo]) -> Vec<String> {
    let mut hosts: Vec<String> = API_HOSTS
        .iter()
        .filter(|(adapter, _)| *adapter == runtime)
        .map(|(_, host)| (*host).to_owned())
        .chain(repos.iter().filter_map(|r| remote_host(&r.url)))
        .collect();
    hosts.sort();
    hosts.dedup();
    hosts
}

pub(crate) async fn listings(
    State(api): State<Api>,
    Query(filter): Query<ListingFilter>,
) -> ApiResult<Json<Value>> {
    let (market, _) = market(&api)?;
    for (value, what) in [
        (&filter.runtime, "runtime"),
        (&filter.region, "region"),
        (&filter.offering, "offering"),
        (&filter.capability, "capability"),
    ] {
        value.as_deref().map(|v| text(v, what)).transpose()?;
    }
    if filter.min_rating.is_some_and(|r| !(0.0..=5.0).contains(&r)) {
        return Err(ApiError::invalid("`min_rating` must be 0–5."));
    }
    Ok(Json(market.listings(&filter).await?))
}

/// Seller side: this ecosystem's own listings, paused ones too.
pub(crate) async fn my_listings(State(api): State<Api>) -> ApiResult<Json<Value>> {
    let (market, _) = market(&api)?;
    Ok(Json(market.my_listings().await?))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ListingStatus {
    status: String,
}

pub(crate) async fn set_listing_status(
    State(api): State<Api>,
    Path(listing): Path<String>,
    Body(body): Body<ListingStatus>,
) -> ApiResult<Json<Value>> {
    let (market, _) = market(&api)?;
    if !matches!(body.status.as_str(), "active" | "paused") {
        return Err(ApiError::invalid("`status` must be `active` or `paused`."));
    }
    let listing = cloud_id(&listing, "listing")?;
    Ok(Json(
        market.set_listing_status(listing, &body.status).await?,
    ))
}

pub(crate) async fn payouts(State(api): State<Api>) -> ApiResult<Json<Value>> {
    let (market, _) = market(&api)?;
    Ok(Json(market.payouts().await?))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct NewPayout {
    points: i64,
    currency: String,
    destination: String,
}

pub(crate) async fn request_payout(
    State(api): State<Api>,
    Body(body): Body<NewPayout>,
) -> ApiResult<(StatusCode, Json<Value>)> {
    let (market, _) = market(&api)?;
    if body.points <= 0 || !matches!(body.currency.as_str(), "usd" | "idr") {
        return Err(ApiError::invalid(
            "Pay out more than 0 points, in `usd` or `idr`.",
        ));
    }
    text(&body.destination, "destination")?;
    let payout = market
        .request_payout(body.points, &body.currency, body.destination.trim())
        .await?;
    Ok((StatusCode::CREATED, Json(payout)))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Rating {
    stars: i64,
    #[serde(default)]
    comment: Option<String>,
}

pub(crate) async fn rate_lease(
    State(api): State<Api>,
    Path(lease): Path<String>,
    Body(body): Body<Rating>,
) -> ApiResult<Json<Value>> {
    let (market, _) = market(&api)?;
    if !(1..=5).contains(&body.stars) {
        return Err(ApiError::invalid("`stars` must be 1–5."));
    }
    let comment = body
        .comment
        .as_deref()
        .map(str::trim)
        .filter(|c| !c.is_empty());
    if comment.is_some_and(|c| c.len() > MAX_REASON) {
        return Err(ApiError::invalid(format!(
            "Keep the comment under {MAX_REASON} characters."
        )));
    }
    let lease = lease_id(&lease)?;
    Ok(Json(market.rate_lease(lease, body.stars, comment).await?))
}

pub(crate) async fn leases(State(api): State<Api>) -> ApiResult<Json<Value>> {
    let (market, _) = market(&api)?;
    Ok(Json(market.leases().await?))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct NewLease {
    task_id: String,
    runtime: String,
    unit: String,
    max_price_points: i64,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    region: Option<String>,
    /// The buyer read that the seller's machine sees the task's code and data.
    #[serde(default)]
    accept_disclosure: bool,
    /// Ecosystem secrets the buyer allows on the seller's machine, by name;
    /// every other one is withheld.
    #[serde(default)]
    secrets: Vec<String>,
}

pub(crate) async fn lease(
    State(api): State<Api>,
    Body(body): Body<NewLease>,
) -> ApiResult<(StatusCode, Json<Value>)> {
    let (market, kernel_url) = market(&api)?;
    if !body.accept_disclosure {
        return Err(ApiError::invalid(
            "Confirm that the seller's machine can see this task's code and data.",
        ));
    }
    lease_new(&api, market, kernel_url, body).await
}

/// Validates `body`, holds its task, leases it and pins it, or drops the hold.
async fn lease_new(
    api: &Api,
    market: Arc<dyn Marketplace>,
    kernel_url: String,
    body: NewLease,
) -> ApiResult<(StatusCode, Json<Value>)> {
    let task_id: TaskId = id(&body.task_id, "task")?;
    text(&body.runtime, "runtime")?;
    if !matches!(body.unit.as_str(), "hour" | "task" | "contract") {
        return Err(ApiError::invalid(
            "`unit` must be `hour`, `task` or `contract`.",
        ));
    }
    if body.secrets.len() > MAX_SECRETS || body.secrets.iter().any(|s| text(s, "secrets").is_err())
    {
        return Err(ApiError::invalid(format!(
            "Allow at most {MAX_SECRETS} secrets, by name."
        )));
    }
    if body.max_price_points <= 0 {
        return Err(ApiError::invalid("`max_price_points` must be above 0."));
    }
    for (value, what) in [(&body.model, "model"), (&body.region, "region")] {
        if let Some(value) = value {
            text(value, what)?;
        }
    }
    let task = api.engine.task(task_id).await?;
    if !matches!(
        task.status,
        sun_core::TaskStatus::Backlog | sun_core::TaskStatus::Todo
    ) {
        return Err(ApiError::conflict(
            "Only a task that hasn't started can run on the marketplace.",
        ));
    }
    if api.engine.task_lease(task_id).await?.is_some() {
        return Err(ApiError::conflict("This task is already leased."));
    }
    // Lease before start: no free host of ours starts it while Sun Cloud answers.
    let terms = TaskLease {
        runtime: Some(body.runtime.clone()),
        secrets: body.secrets.clone(),
        ..TaskLease::default()
    };
    api.engine.hold_for_lease(task_id, terms).await?;
    let leased = lease_held(api, market, kernel_url, body, task).await;
    if leased.is_err() {
        let _ = api.engine.release_lease_hold(task_id).await;
        api.queue.wake();
    }
    leased
}

/// Leases the held `task` on Sun Cloud and pins it to the seller's VM.
async fn lease_held(
    api: &Api,
    market: Arc<dyn Marketplace>,
    kernel_url: String,
    body: NewLease,
    task: sun_core::model::Task,
) -> ApiResult<(StatusCode, Json<Value>)> {
    let task_id = task.id;
    // The repos its run works in: its latest run's, or the ones its text
    // names before it ran.
    let repos = api.engine.lease_repos(task_id).await?;
    let network = allowlist(&body.runtime, &repos);
    let join_token = api.hosts.issue_token_for(Some(task_id))?;
    let request = LeaseRequest {
        task_ref: task_id.to_string(),
        runtime: body.runtime,
        unit: body.unit,
        max_price_points: body.max_price_points,
        model: body.model,
        region: body.region,
        kernel_url,
        join_token,
        capability: None,
        min_rating: None,
    };
    let lease = market.lease(&request).await?;
    let (Some(lease_id), Some(listing_id)) = (lease["id"].as_str(), lease["listing_id"].as_str())
    else {
        return Err(ApiError::new(
            StatusCode::BAD_GATEWAY,
            "protocol",
            "Sun Cloud returned a lease without an id.",
        ));
    };
    let listings = market
        .listings(&Default::default())
        .await
        .unwrap_or_default();
    let template = listings
        .as_array()
        .and_then(|all| all.iter().find(|l| l["id"] == listing_id))
        .and_then(|l| l["vm_template"].as_str())
        .unwrap_or(DEFAULT_TEMPLATE)
        .to_owned();
    let vm = ExecEnvSpec::Vm(VmSpec {
        template,
        memory_mb: None,
        cpus: None,
        network: Some(network),
        keep: false,
    });
    if let Err(e) = api.engine.lease_task(task_id, lease_id, &vm).await {
        // Not pinned here: end the lease, so the escrow isn't left held.
        let _ = market.close(lease_id).await;
        return Err(e.into());
    }
    Ok((StatusCode::CREATED, Json(lease)))
}

/// How long a failover lease that Sun Cloud refused keeps its task held
/// before the queue may try again.
// ponytail: one fixed back-off; scale it per refusal if clouds get hammered.
const FAILOVER_RETRY: std::time::Duration = std::time::Duration::from_secs(60);

impl Api {
    /// Runs forever (**Observer** on the event log): leases each task the
    /// queue failed over to a `market:` chain entry ([`sun_engine::MARKET_FAILOVER`])
    /// per task, at most the policy's price, with no secret allowed (the
    /// ecosystem policy stands in for the disclosure). A refused lease keeps
    /// the hold for [`FAILOVER_RETRY`], then gives the task back to the queue.
    /// `sun serve` spawns it under Sun Cloud.
    pub async fn follow_market_failover(self) {
        let mut events = self.engine.subscribe();
        loop {
            let event = match events.recv().await {
                Ok(event) => event,
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
            };
            if event.kind != sun_engine::MARKET_FAILOVER {
                continue;
            }
            let p = &event.payload;
            let (Some(task), Some(runtime), Some(max)) = (
                p["task_id"].as_str(),
                p["runtime"].as_str(),
                p["max_price_points"].as_i64(),
            ) else {
                continue;
            };
            let body = NewLease {
                task_id: task.to_owned(),
                runtime: runtime.to_owned(),
                unit: "task".into(),
                max_price_points: max,
                model: None,
                region: None,
                accept_disclosure: true,
                secrets: Vec::new(),
            };
            let api = self.clone();
            tokio::spawn(async move { api.lease_failover(body).await });
        }
    }

    async fn lease_failover(self, body: NewLease) {
        let Ok(task_id) = id::<TaskId>(&body.task_id, "task") else {
            return;
        };
        let leased = match (market(&self), self.engine.task(task_id).await) {
            (Ok((market, url)), Ok(task)) => {
                lease_held(&self, market, url, body, task).await.map(|_| ())
            }
            (Err(e), _) => Err(e),
            (_, Err(e)) => Err(e.into()),
        };
        if let Err(e) = leased {
            eprintln!("sun: marketplace failover of task {task_id}: {e:?}");
            tokio::time::sleep(FAILOVER_RETRY).await;
            let _ = self.engine.release_lease_hold(task_id).await;
            self.queue.wake();
        }
    }
}

pub(crate) async fn policy(State(api): State<Api>) -> ApiResult<Json<MarketPolicy>> {
    Ok(Json(api.engine.market_policy().await?))
}

pub(crate) async fn set_policy(
    State(api): State<Api>,
    Body(policy): Body<MarketPolicy>,
) -> ApiResult<Json<MarketPolicy>> {
    api.engine.set_market_policy(&policy).await?;
    api.queue.wake();
    Ok(Json(policy))
}

pub(crate) async fn accept(
    State(api): State<Api>,
    Path(lease): Path<String>,
) -> ApiResult<Json<Value>> {
    let (market, _) = market(&api)?;
    Ok(Json(market.accept(lease_id(&lease)?).await?))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Dispute {
    reason: String,
}

pub(crate) async fn dispute(
    State(api): State<Api>,
    Path(lease): Path<String>,
    Body(body): Body<Dispute>,
) -> ApiResult<Json<Value>> {
    let (market, _) = market(&api)?;
    let reason = body.reason.trim();
    if reason.is_empty() || reason.len() > MAX_REASON {
        return Err(ApiError::invalid(format!(
            "Say what went wrong in 1–{MAX_REASON} characters."
        )));
    }
    Ok(Json(market.dispute(lease_id(&lease)?, reason).await?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remote_hosts_and_allowlist() {
        assert_eq!(
            remote_host("https://github.com/a/b.git").as_deref(),
            Some("github.com")
        );
        assert_eq!(
            remote_host("git@GitLab.com:a/b.git").as_deref(),
            Some("gitlab.com")
        );
        assert_eq!(
            remote_host("ssh://git@git.example:22/x").as_deref(),
            Some("git.example")
        );
        let repo = |url: &str| Repo {
            id: sun_core::RepoId::generate(),
            workspace_id: sun_core::WorkspaceId::MAIN,
            url: url.into(),
            default_branch: "main".into(),
            gate_config: Value::Null,
            description: String::new(),
            hook_id: None,
        };
        let repos = [repo("https://github.com/a/b"), repo("git@github.com:a/c")];
        assert_eq!(
            allowlist("claude-code", &repos),
            ["api.anthropic.com", "github.com"]
        );
        assert!(lease_id("lea_01J").is_ok());
        assert!(lease_id("../admin").is_err());
    }
}
