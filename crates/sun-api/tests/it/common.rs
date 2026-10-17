//! A real kernel (store, engine, queue, API) in a temp dir, shared by the
//! integration tests.
#![allow(clippy::unwrap_used)]

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use axum::Router;
use serde_json::{Value, json};
use sun_api::{Api, ApiConfig, HostHub, PresetUrls};
use sun_core::entitlement::{Entitlement, EntitlementProvider, EntitlementStatus, Limits, Plan};
use sun_core::model::{Host, Mail, SmtpSettings};
use sun_core::{HostId, Mailer, Marketplace, ModCall, ModLink, Result, WorkspaceId};
use sun_engine::{Caps, Engine, EngineConfig, Principal, Queue, QueueConfig, Slot, WatchdogConfig};
use sun_store::{HostRepo, Store, WorkspaceItem, WorkspaceRepo};

pub const TOKEN: &str = "test-token";
pub const SECRET: &str = "host-secret";

struct NoMods;

#[async_trait]
impl ModLink for NoMods {
    async fn call(&self, _: ModCall) -> Result<Value> {
        Ok(json!({}))
    }
}

/// A [`Mailer`] that keeps every mail (and the SMTP password it got)
/// instead of sending it.
#[derive(Default)]
pub struct FakeMailer {
    pub sent: std::sync::Mutex<Vec<(Mail, Option<String>)>>,
}

#[async_trait]
impl Mailer for FakeMailer {
    async fn send(&self, _: &SmtpSettings, password: Option<&str>, mail: &Mail) -> Result<()> {
        self.sent
            .lock()
            .unwrap()
            .push((mail.clone(), password.map(str::to_owned)));
        Ok(())
    }
}

impl FakeMailer {
    /// The last mail sent.
    pub fn last(&self) -> Mail {
        self.sent.lock().unwrap().last().unwrap().0.clone()
    }
}

pub struct Kernel {
    pub app: Router,
    /// The API behind `app` (e.g. for `sun serve`'s first-run seeding).
    pub api: sun_api::Api,
    pub host: HostId,
    pub store: Store,
    /// Agent sessions' MCP tokens (`/mcp`, `/mcp/{server}`).
    pub mcp: Arc<sun_context::McpSessions>,
    /// Every mail the kernel sent.
    pub mails: Arc<FakeMailer>,
    _dir: tempfile::TempDir,
    _hang: HangGuard,
}

/// A test still running after this is hung: it aborts instead of stalling the run.
const HANG: Duration = Duration::from_secs(180);

/// Aborts the test binary when the test that built a [`Kernel`] outlives [`HANG`],
/// naming the test. A plain thread, not a tokio timeout: a deadlock that blocks a
/// current-thread runtime would stop that timeout from ever firing.
pub struct HangGuard(
    #[expect(dead_code, reason = "held only so its drop ends the wait")] std::sync::mpsc::Sender<()>,
);

/// One request through `app`, failing the test (naming the request) when it
/// gets no response within a minute instead of waiting forever.
pub async fn respond(
    app: &Router,
    request: axum::http::Request<axum::body::Body>,
) -> axum::response::Response {
    use tower::ServiceExt;
    let what = format!("{} {}", request.method(), request.uri());
    tokio::time::timeout(Duration::from_secs(60), app.clone().oneshot(request))
        .await
        .unwrap_or_else(|_| panic!("{what}: no response in 60 s"))
        .unwrap()
}

fn hang_guard() -> HangGuard {
    let (done, wait) = std::sync::mpsc::channel::<()>();
    let test = std::thread::current().name().unwrap_or("a test").to_owned();
    std::thread::spawn(move || {
        if let Err(std::sync::mpsc::RecvTimeoutError::Timeout) = wait.recv_timeout(HANG) {
            eprintln!("{test} still runs after {HANG:?}: hung, aborting the test binary");
            std::process::abort();
        }
    });
    HangGuard(done)
}

pub async fn kernel() -> Kernel {
    kernel_with(None).await
}

/// A kernel whose Google and GitHub presets live at `presets` (a mock).
pub async fn kernel_with(presets: Option<PresetUrls>) -> Kernel {
    build(presets, Arc::new(NoMods), None, Caps::default()).await
}

/// A kernel with only `features` on (sun.yaml `features:`).
pub async fn kernel_with_features(features: sun_api::Features) -> Kernel {
    restarted(kernel().await, features).await
}

/// `k`'s store behind a new API (with only `features` on), as after a
/// kernel restart.
pub async fn restarted(k: Kernel, features: sun_api::Features) -> Kernel {
    let (app, api) = app_with(
        k.store.clone(),
        k.host,
        k._dir.path(),
        None,
        Arc::new(NoMods),
        k.mcp.clone(),
        k.mails.clone(),
        None,
        Caps::default(),
        features,
    )
    .await;
    Kernel { app, api, ..k }
}

/// A kernel on Sun Cloud's marketplace `market`, at `https://buyer.example`.
pub async fn kernel_with_market(market: Arc<dyn Marketplace>) -> Kernel {
    build(None, Arc::new(NoMods), Some(market), Caps::default()).await
}

/// A kernel whose engine and webhooks use `mods`.
pub async fn kernel_with_mods(mods: Arc<dyn ModLink>) -> Kernel {
    build(None, mods, None, Caps::default()).await
}

/// A kernel whose queue admits within `caps` (its host is named `box`).
pub async fn kernel_with_caps(caps: Caps) -> Kernel {
    build(None, Arc::new(NoMods), None, caps).await
}

async fn build(
    presets: Option<PresetUrls>,
    mods: Arc<dyn ModLink>,
    market: Option<Arc<dyn Marketplace>>,
    caps: Caps,
) -> Kernel {
    let hang = hang_guard();
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).await.unwrap();
    // The engine places every session on this host; enrol it up front.
    let host = HostId::generate();
    let mut tx = store.begin().await.unwrap();
    tx.insert_host(&Host {
        id: host,
        name: "box".into(),
        capabilities: json!({}),
        enrolled_by: None,
    })
    .await
    .unwrap();
    tx.link(WorkspaceId::MAIN, WorkspaceItem::Host(host))
        .await
        .unwrap();
    tx.commit().await.unwrap();
    let mcp = Arc::new(sun_context::McpSessions::new());
    let mails = Arc::new(FakeMailer::default());
    let (app, api) = app(
        store.clone(),
        host,
        dir.path(),
        presets,
        mods,
        mcp.clone(),
        mails.clone(),
        market,
        caps,
    )
    .await;
    Kernel {
        app,
        api,
        host,
        store,
        mcp,
        mails,
        _dir: dir,
        _hang: hang,
    }
}

/// A new engine, queue and API on `k`'s store, as after a kernel restart.
pub async fn restart(k: &Kernel, presets: Option<PresetUrls>) -> Router {
    app(
        k.store.clone(),
        k.host,
        k._dir.path(),
        presets,
        Arc::new(NoMods),
        k.mcp.clone(),
        k.mails.clone(),
        None,
        Caps::default(),
    )
    .await
    .0
}

#[allow(clippy::too_many_arguments)]
async fn app(
    store: Store,
    host: HostId,
    dir: &std::path::Path,
    presets: Option<PresetUrls>,
    mods: Arc<dyn ModLink>,
    mcp: Arc<sun_context::McpSessions>,
    mails: Arc<FakeMailer>,
    market: Option<Arc<dyn Marketplace>>,
    caps: Caps,
) -> (Router, Api) {
    let all = sun_api::Features::default();
    app_with(
        store, host, dir, presets, mods, mcp, mails, market, caps, all,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn app_with(
    store: Store,
    host: HostId,
    dir: &std::path::Path,
    presets: Option<PresetUrls>,
    mods: Arc<dyn ModLink>,
    mcp: Arc<sun_context::McpSessions>,
    mails: Arc<FakeMailer>,
    market: Option<Arc<dyn Marketplace>>,
    caps: Caps,
    features: sun_api::Features,
) -> (Router, Api) {
    let hosts = HostHub::default();
    let (engine, _) = Engine::start(
        store.clone(),
        Arc::new(hosts.clone()),
        mods.clone(),
        EngineConfig {
            allow_bypass: false,
            host,
            adapter: "fake".into(),
            delivery_timeout: Duration::from_secs(5),
            watchdog: WatchdogConfig::default(),
        },
    )
    .await
    .unwrap();
    // Git host merges (`pr_merged`) from the mods' events, as `sun serve` does.
    tokio::spawn(engine.clone().follow_merges());
    let (queue, _) = Queue::start(
        store.clone(),
        engine.clone(),
        QueueConfig {
            slot: Slot {
                runtime: "fake".into(),
                host,
            },
            caps,
        },
    );
    let config = ApiConfig {
        token: TOKEN.into(),
        host_secret: SECRET.into(),
        principal: Principal {
            user: "me".into(),
            roles: vec!["admin".into()],
        },
        public_url: "http://localhost".into(),
    };
    // A kernel on Sun Cloud's marketplace is connected to Sun Cloud.
    let api = Api::new(engine, queue, hosts, config).with_sun_cloud(market.is_some());
    let api = match market {
        Some(market) => api.with_marketplace(market, "https://buyer.example"),
        None => api,
    };
    let api = match presets {
        Some(urls) => api.with_preset_urls(urls),
        None => api,
    };
    let api = api
        .with_mods(mods)
        .with_mailer(mails, "https://sun.test")
        .with_features(features)
        .with_preview_host(host)
        .with_mcp(mcp)
        .with_catalog(sun_catalog::Catalog::open(dir.join("data")).unwrap())
        .with_entitlements(Arc::new(FilePlans(dir.join(PLANS_FILE))));
    (sun_api::router(api.clone()), api)
}

/// A repo row of `workspace` at `url` (a local path the host can clone),
/// inserted directly: the API takes only http, ssh and git URLs. A run
/// picks it up when the task text names it (the path's last segment).
pub async fn local_repo(k: &Kernel, workspace: sun_core::WorkspaceId, url: &str) -> String {
    use sun_store::RepoRepo;
    let repo = sun_core::model::Repo {
        id: sun_core::RepoId::generate(),
        workspace_id: workspace,
        url: url.into(),
        default_branch: "main".into(),
        gate_config: json!({}),
        description: String::new(),
        hook_id: None,
    };
    let mut tx = k.store.begin().await.unwrap();
    tx.insert_repo(&repo).await.unwrap();
    tx.commit().await.unwrap();
    repo.id.to_string()
}

/// The test plans of a kernel: `{"<workspace id>": Limits}`, each a
/// Business plan from the cloud; a workspace not in it is Community.
const PLANS_FILE: &str = "plans.json";

/// Reads [`PLANS_FILE`] on every call, so a test changes a plan mid-way.
struct FilePlans(std::path::PathBuf);

#[async_trait]
impl EntitlementProvider for FilePlans {
    async fn for_workspace(&self, workspace: WorkspaceId) -> Entitlement {
        let plans: HashMap<String, Limits> = std::fs::read(&self.0)
            .map(|bytes| serde_json::from_slice(&bytes).unwrap())
            .unwrap_or_default();
        match plans.get(&workspace.to_string()) {
            Some(limits) => Entitlement {
                plan: Plan::Business,
                paid: true,
                limits: *limits,
                status: EntitlementStatus::Cloud {
                    valid_until: chrono::Utc::now() + chrono::Duration::minutes(5),
                },
            },
            None => Entitlement::community(EntitlementStatus::Community),
        }
    }
}

/// Puts `workspace` of `k` on a Business plan with `limits`.
pub fn plan(k: &Kernel, workspace: WorkspaceId, limits: Limits) {
    let path = k._dir.path().join(PLANS_FILE);
    let mut plans: HashMap<String, Limits> = std::fs::read(&path)
        .map(|bytes| serde_json::from_slice(&bytes).unwrap())
        .unwrap_or_default();
    plans.insert(workspace.to_string(), limits);
    std::fs::write(path, serde_json::to_vec(&plans).unwrap()).unwrap();
}
