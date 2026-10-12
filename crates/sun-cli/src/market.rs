//! Sun Cloud marketplace, kernel side.
//!
//! - **Buyer:** [`CloudMarket`] is the kernel's [`Marketplace`] (**Adapter**
//!   over `/marketplace/v1`); the API mints the task's join token and leases.
//! - **Seller:** [`Seller`] is the polling loop behind
//!   `sun host join --marketplace`: it takes this host's dispatches, joins the
//!   buyer kernel with each one's single-use token (a marketplace host: reset
//!   VMs with a network allowlist only) and stops that host once its lease closes.
//!   [`HostKey`] proves the seller holds the host it lists ([`create_listing`]).
//! - **Mods:** [`market_mod`] + [`install_mod`] buy (if paid) and install a
//!   marketplace mod in one Sun Cloud call, verified against the registry index.

use std::collections::HashMap;
use std::future::Future;
use std::path::Path;
use std::sync::Arc;

use async_trait::async_trait;
use ed25519_dalek::{Signer, SigningKey};
use serde::Deserialize;
use serde_json::{Value, json};
use sun_core::model::ModInstall;
use sun_core::{
    BoxStream, Error, LeaseRequest, ListingFilter, Marketplace, ModCall, ModEvent, ModLink, Result,
};
use sun_engine::{MARKET_MOD, RELEASE_LEASE};
use tokio::task::JoinHandle;

use crate::cloud::CloudClient;
use crate::mods::hex;

const MARKET: &str = "/marketplace/v1";
/// Domain of the host ownership proof (Sun Cloud's `host_message`).
const HOST_DOMAIN: &str = "sun-marketplace-host-v1";
/// The host key's file in the data dir.
pub const HOST_KEY_FILE: &str = "host.key";

/// `k=v&...` of the set fields, percent-encoded.
fn query(pairs: &[(&str, Option<String>)]) -> String {
    let encode = |value: &str| -> String {
        value
            .bytes()
            .map(|b| match b {
                b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                    char::from(b).to_string()
                }
                _ => format!("%{b:02X}"),
            })
            .collect()
    };
    let set: Vec<String> = pairs
        .iter()
        .filter_map(|(key, value)| Some(format!("{key}={}", encode(value.as_deref()?))))
        .collect();
    match set.is_empty() {
        true => String::new(),
        false => format!("?{}", set.join("&")),
    }
}

/// The kernel's marketplace account on Sun Cloud.
pub struct CloudMarket(Arc<CloudClient>);

impl CloudMarket {
    /// Leases as the ecosystem `client` authenticates.
    pub fn new(client: CloudClient) -> Self {
        Self(Arc::new(client))
    }
}

#[async_trait]
impl Marketplace for CloudMarket {
    async fn listings(&self, filter: &ListingFilter) -> Result<Value> {
        let query = query(&[
            ("runtime", filter.runtime.clone()),
            ("region", filter.region.clone()),
            ("offering", filter.offering.clone()),
            ("capability", filter.capability.clone()),
            ("min_rating", filter.min_rating.map(|r| r.to_string())),
        ]);
        self.0.get(format!("{MARKET}/listings{query}")).await
    }

    async fn my_listings(&self) -> Result<Value> {
        self.0.get(format!("{MARKET}/listings/mine")).await
    }

    async fn set_listing_status(&self, listing_id: &str, status: &str) -> Result<Value> {
        let body = json!({ "status": status });
        self.0
            .post(format!("{MARKET}/listings/{listing_id}/status"), Some(body))
            .await
    }

    async fn payouts(&self) -> Result<Value> {
        self.0.get(format!("{MARKET}/payouts")).await
    }

    async fn request_payout(
        &self,
        points: i64,
        currency: &str,
        destination: &str,
    ) -> Result<Value> {
        let body = json!({ "points": points, "currency": currency, "destination": destination });
        self.0.post(format!("{MARKET}/payouts"), Some(body)).await
    }

    async fn rate_lease(&self, lease_id: &str, stars: i64, comment: Option<&str>) -> Result<Value> {
        let body = json!({ "stars": stars, "comment": comment });
        self.0
            .post(format!("{MARKET}/leases/{lease_id}/rating"), Some(body))
            .await
    }

    async fn leases(&self) -> Result<Value> {
        self.0.get(format!("{MARKET}/leases")).await
    }

    async fn lease(&self, request: &LeaseRequest) -> Result<Value> {
        self.0
            .post(format!("{MARKET}/leases"), Some(json!(request)))
            .await
    }

    async fn close(&self, lease_id: &str) -> Result<Value> {
        self.0
            .post(format!("{MARKET}/leases/{lease_id}/close"), None)
            .await
    }

    async fn accept(&self, lease_id: &str) -> Result<Value> {
        self.0
            .post(format!("{MARKET}/leases/{lease_id}/accept"), None)
            .await
    }

    async fn dispute(&self, lease_id: &str, reason: &str) -> Result<Value> {
        let body = json!({ "reason": reason });
        self.0
            .post(format!("{MARKET}/leases/{lease_id}/dispute"), Some(body))
            .await
    }

    async fn release(&self, lease_id: &str) -> Result<Value> {
        self.0
            .post(format!("{MARKET}/leases/{lease_id}/release"), None)
            .await
    }
}

/// The engine's [`ModLink`] under Sun Cloud (**Adapter**): outbox rows for
/// the [`MARKET_MOD`] pseudo-mod (`sun_cloud.release_lease {lease_id}`, a
/// failed mid-run market move) go to Sun Cloud's marketplace; every other
/// call goes to the installed mods.
pub struct MarketMods {
    mods: Arc<dyn ModLink>,
    market: Arc<dyn Marketplace>,
}

impl MarketMods {
    /// Routes [`MARKET_MOD`] calls to `market`, the rest to `mods`.
    pub fn new(mods: Arc<dyn ModLink>, market: Arc<dyn Marketplace>) -> Self {
        Self { mods, market }
    }

    async fn release(&self, params: &Value) -> Result<Value> {
        let lease_id = params["lease_id"]
            .as_str()
            .filter(|id| id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_'))
            .filter(|id| (1..=64).contains(&id.len()))
            .ok_or_else(|| Error::InvalidInput("release_lease: a lease_id".into()))?;
        self.market.release(lease_id).await
    }
}

#[async_trait]
impl ModLink for MarketMods {
    async fn call(&self, call: ModCall) -> Result<Value> {
        if call.mod_name != MARKET_MOD {
            return self.mods.call(call).await;
        }
        match call.method.as_str() {
            RELEASE_LEASE => self.release(&call.params).await,
            // Releasing is idempotent on Sun Cloud: reconcile by releasing again.
            "reconcile" if call.params["method"] == RELEASE_LEASE => {
                let result = self.release(&call.params["params"]).await?;
                Ok(json!({ "delivered": true, "result": result }))
            }
            other => Err(Error::InvalidInput(format!(
                "{MARKET_MOD} has no method `{other}`"
            ))),
        }
    }

    async fn subscribe(&self, mod_name: &str, triggers: Vec<String>) -> Result<()> {
        self.mods.subscribe(mod_name, triggers).await
    }

    fn events(&self) -> BoxStream<'static, ModEvent> {
        self.mods.events()
    }
}

/// This machine's runtime-host key (`<data dir>/host.key`, a hex ed25519
/// seed, owner-only, created on first use). Its public key is the host's
/// marketplace `host_id`; signing [`HostKey::listing_proof`] proves a seller
/// holds the host it lists, so no one else can list it.
pub struct HostKey(SigningKey);

impl HostKey {
    /// Loads `<data_dir>/host.key`, creating it on first use.
    pub fn load_or_create(data_dir: &Path) -> anyhow::Result<Self> {
        let path = data_dir.join(HOST_KEY_FILE);
        if !path.exists() {
            crate::config::write_private(&path, crate::config::random_hex()?.as_bytes())?;
            eprintln!("created host key {} (back it up)", path.display());
        }
        let seed = crate::mods::parse_key(&std::fs::read_to_string(&path)?)?;
        Ok(Self::from_seed(&seed))
    }

    /// The key for `seed`.
    pub fn from_seed(seed: &[u8; 32]) -> Self {
        Self(SigningKey::from_bytes(seed))
    }

    /// The `host_id` listings and dispatches name: the hex public key.
    pub fn id(&self) -> String {
        hex(self.0.verifying_key().as_bytes())
    }

    /// Hex signature of `sun-marketplace-host-v1\n<seller ecosystem>\n<host id>`.
    pub fn listing_proof(&self, seller_ecosystem: &str) -> String {
        let message = format!("{HOST_DOMAIN}\n{seller_ecosystem}\n{}", self.id());
        hex(&self.0.sign(message.as_bytes()).to_bytes())
    }
}

/// Lists runtime capacity on this host: `listing` is Sun Cloud's
/// `NewListing` without `host_id` and `host_signature`, which come from `key`
/// (signed for the ecosystem Sun Cloud says this kernel is).
pub async fn create_listing(
    cloud: &Arc<CloudClient>,
    key: &HostKey,
    listing: Value,
) -> Result<Value> {
    let Value::Object(mut listing) = listing else {
        return Err(Error::InvalidInput("a listing is a JSON object".into()));
    };
    let me = cloud.get(format!("{MARKET}/whoami")).await?;
    // `workspace`: a Sun Cloud from before the rename.
    let ecosystem = me["ecosystem"]
        .as_str()
        .or_else(|| me["workspace"].as_str())
        .ok_or_else(|| Error::Protocol("Sun Cloud whoami: no ecosystem".into()))?;
    listing.insert("host_id".into(), json!(key.id()));
    listing.insert("host_signature".into(), json!(key.listing_proof(ecosystem)));
    cloud
        .post(format!("{MARKET}/listings"), Some(Value::Object(listing)))
        .await
}

/// A mod as the registry index lists it, at one version.
#[derive(Debug, Clone)]
pub struct MarketMod {
    /// Mod name.
    pub name: String,
    /// The version to install.
    pub version: String,
    /// `free`, `paid` or `exclusive`.
    pub pricing: String,
    /// Hex key every version is signed with.
    pub owner_key: String,
    /// Hex SHA-256 the package must have.
    pub package_digest: String,
    /// What it asks for; installing grants exactly these.
    pub permissions: Vec<String>,
}

/// Mod `name` from Sun Cloud's registry index, at `version` or its newest
/// version that isn't yanked.
pub async fn market_mod(
    cloud: &Arc<CloudClient>,
    name: &str,
    version: Option<&str>,
) -> Result<MarketMod> {
    let index = cloud.get("/registry/v1/index".into()).await?;
    let entry = index
        .as_array()
        .into_iter()
        .flatten()
        .find(|e| e["name"] == name)
        .ok_or_else(|| Error::NotFound(format!("mod {name} is not in the marketplace")))?;
    let versions = entry["versions"].as_array().into_iter().flatten();
    let chosen = match version {
        Some(version) => versions.into_iter().find(|v| v["version"] == version),
        None => versions.filter(|v| v["yanked"] != true).max_by_key(|v| {
            v["version"]
                .as_str()
                .and_then(|v| semver::Version::parse(v).ok())
        }),
    }
    .ok_or_else(|| Error::NotFound(format!("mod {name}: no such installable version")))?;
    let text = |value: &Value| value.as_str().unwrap_or_default().to_owned();
    Ok(MarketMod {
        name: name.to_owned(),
        version: text(&chosen["version"]),
        pricing: text(&entry["pricing"]),
        owner_key: text(&entry["owner_key"]),
        package_digest: text(&chosen["package_digest"]),
        permissions: chosen["permissions"]
            .as_array()
            .into_iter()
            .flatten()
            .map(text)
            .collect(),
    })
}

/// One-click install: Sun Cloud buys `listed` if it is paid and not bought
/// yet (402 → `Forbidden` without the points), and serves the package in the
/// same call; it installs through [`crate::installer::install_archive`]
/// (checked against the index) with its listed permissions.
pub async fn install_mod(
    cloud: &Arc<CloudClient>,
    mods_dir: &Path,
    listed: &MarketMod,
) -> Result<ModInstall> {
    let (name, version) = (&listed.name, &listed.version);
    let path = format!("{MARKET}/mods/{name}/{version}/install");
    let archive = cloud
        .post_bytes(path, crate::installer::MAX_PACKAGE)
        .await?;
    crate::installer::install_archive(
        &archive,
        mods_dir,
        name,
        version,
        &listed.package_digest,
        &listed.owner_key,
        &listed.permissions,
    )
}

/// A leased task for one of the seller's hosts (Sun Cloud's dispatch).
#[derive(Debug, Clone, Deserialize)]
pub struct Dispatch {
    /// The lease.
    pub lease_id: String,
    /// The seller's host meant to run it (the listing's `host_id`).
    pub host_id: String,
    /// The buyer's task id.
    pub task_ref: String,
    /// Lima template of the task VM.
    pub vm_template: String,
    /// The buyer kernel to join.
    pub kernel_url: String,
    /// Single-use join token for it.
    pub join_token: String,
}

/// The seller's polling loop for one host (**State machine** per lease:
/// dispatched → joined → stopped when the lease closes).
pub struct Seller {
    cloud: Arc<CloudClient>,
    host_id: String,
    /// Joined hosts by lease; aborting one stops serving the buyer.
    running: HashMap<String, JoinHandle<()>>,
}

impl Seller {
    /// Serves the dispatches of `host_id` (the listings' `host_id`).
    pub fn new(cloud: CloudClient, host_id: &str) -> Self {
        Self {
            cloud: Arc::new(cloud),
            host_id: host_id.to_owned(),
            running: HashMap::new(),
        }
    }

    /// Leases this host is serving now.
    pub fn serving(&self) -> Vec<String> {
        self.running.keys().cloned().collect()
    }

    /// One poll: stops the hosts whose lease closed (returned, so their dirs
    /// can go), then joins each new dispatch with `join`, which returns the
    /// running host. A dispatch that can't join is closed (the buyer is refunded).
    pub async fn tick<F, Fut>(&mut self, mut join: F) -> Result<Vec<String>>
    where
        F: FnMut(Dispatch) -> Fut,
        Fut: Future<Output = Result<JoinHandle<()>>>,
    {
        let leases = self.cloud.get(format!("{MARKET}/leases")).await?;
        let open = |id: &str| {
            leases.as_array().is_some_and(|all| {
                all.iter()
                    .any(|l| l["id"] == id && l["status"].as_str() == Some("open"))
            })
        };
        let closed: Vec<String> = self
            .running
            .keys()
            .filter(|id| !open(id))
            .cloned()
            .collect();
        for id in &closed {
            if let Some(host) = self.running.remove(id) {
                host.abort();
            }
        }
        let dispatches = self.cloud.get(format!("{MARKET}/dispatches")).await?;
        let dispatches: Vec<Dispatch> = serde_json::from_value(dispatches)
            .map_err(|e| Error::Protocol(format!("Sun Cloud dispatches: {e}")))?;
        for dispatch in dispatches {
            if dispatch.host_id != self.host_id || self.running.contains_key(&dispatch.lease_id) {
                continue;
            }
            let id = dispatch.lease_id.clone();
            match join(dispatch).await {
                Ok(host) => {
                    self.running.insert(id.clone(), host);
                    self.cloud
                        .post(format!("{MARKET}/leases/{id}/dispatched"), None)
                        .await?;
                }
                Err(e) => {
                    tracing::warn!("marketplace lease {id}: joining the buyer failed: {e}");
                    self.cloud
                        .post(format!("{MARKET}/leases/{id}/close"), None)
                        .await?;
                }
            }
        }
        Ok(closed)
    }
}
