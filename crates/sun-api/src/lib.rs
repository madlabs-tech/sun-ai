//! # sun-api — HTTP API, SSE timeline and the kernel side of the host protocol
//!
//! **Responsibility:** the kernel's network edge. REST under `/api/v1`
//! (JSON, snake_case), the live run timeline and task activity as SSE
//! (`sse`), the task lifecycle (`tasks`), the local auth token,
//! the `/host` WebSocket with its [`HostHub`] (`sun_core::HostLink` impl), and
//! design-canvas previews proxied through the host socket (`preview`),
//! the live device view relayed over it (`device`),
//! and the embedded web UI (`web/dist`, pre-compressed) at `/`.
//! Every UI action exists here first. The routes are in
//! `routes`; the host frames are `sun_core::host_protocol`.
//!
//! **Contract:**
//! - All input is validated here (ids, bodies, sizes) before it reaches the
//!   engine; errors are `{"error": {code, message}}` with a plain message.
//! - Reads and writes go through `sun_engine::Engine` (the API never touches
//!   the store) and `sun_engine::Queue`; run transitions stay on the engine task.
//! - Secrets (token, host keys) are compared in constant time and never logged.
//! - Auth (`auth`): the local token (a deployment owner), or a member's
//!   session (a password, an emailed code or SSO); sessions are stored.
//!   People sign up (when the deployment allows it) or are invited into a
//!   workspace; mail goes through the injected `sun_core::Mailer`. A member
//!   acts in each workspace by their role there: `GET`s need membership,
//!   other methods `run` there, and each workspace verb its permission; the
//!   deployment's own settings and accounts need a deployment owner. Logins,
//!   role changes and approvals are audited.
//!
//! **May depend on:** `sun-core`, `sun-engine`, `sun-context`, `sun-catalog`.
//!
//! ## Wiring (`sun-cli`, **Dependency injection**)
//! ```ignore
//! let hosts = HostHub::default();
//! let (engine, _) = Engine::start(store, Arc::new(hosts.clone()), mods.clone(), config).await?;
//! let (queue, _) = Queue::start(store, engine.clone(), queue_config);
//! let app = sun_api::router(Api::new(engine, queue, hosts, api_config).with_mods(mods));
//! axum::serve(listener, app).await?;
//! ```

mod agents;
mod auth;
mod billing;
mod catalog;
mod corrections;
mod device;
mod entitlement;
mod error;
mod evals;
mod features;
mod hosts;
mod knowledge;
mod marketplace;
mod mcp;
mod memory;
mod pins;
mod preview;
mod projects;
mod pull_requests;
mod quality;
mod routes;
mod secrets;
mod sse;
mod tasks;
mod ui;
mod vcs;
mod webhooks;
mod wire;
mod workspace_access;
mod workspaces;

use std::sync::Arc;

use axum::Router;
use sun_core::{HostId, Mailer, Marketplace, ModInstaller, ModLink};
use sun_engine::{Engine, Principal, Queue};

pub use auth::PresetUrls;
pub use billing::CloudBilling;
pub use entitlement::CloudPolicy;
pub use features::Features;
pub use hosts::{HostHub, RuntimesModel, host_key_for};

/// Settings for the API, supplied by `sun-cli`.
#[derive(Debug, Clone)]
pub struct ApiConfig {
    /// The kernel token at boot (the CLI's `Authorization: Bearer`). Its
    /// holder is an owner. It changes once, when the first-run owner sets
    /// their own password ([`Api::with_token_rotation`]).
    pub token: String,
    /// Kernel secret host keys are derived from (`sha256(secret ‖ host_id)`).
    /// Changing it revokes every enrolled host.
    pub host_secret: String,
    /// Who the token holder is when deciding approvals (`who:` matching).
    pub principal: Principal,
    /// The URL browsers reach the kernel at, e.g. `https://sun.example.com`.
    /// SSO callbacks are `<public_url>/api/v1/auth/sso/{id}/callback`;
    /// `https://` makes the sign-in cookies `Secure`.
    pub public_url: String,
}

/// Writes a new kernel token where the CLI reads it (`kernel.json`) and
/// returns it; an error says why it couldn't.
pub type TokenRotation = Arc<dyn Fn() -> Result<String, String> + Send + Sync>;

/// Shared state of every handler. Cheap to clone.
#[derive(Clone)]
pub struct Api {
    engine: Engine,
    queue: Queue,
    hosts: HostHub,
    config: Arc<ApiConfig>,
    /// The kernel token now: [`ApiConfig::token`] until rotated.
    token: Arc<std::sync::RwLock<String>>,
    /// Rotates the kernel token (`with_token_rotation`); none: never rotated.
    rotate_token: Option<TokenRotation>,
    catalog: Option<Arc<sun_catalog::Catalog>>,
    /// Webhook deliveries go to these mods (`with_mods`).
    mods: Option<Arc<dyn ModLink>>,
    /// Agent sessions' `sun` MCP tokens (`with_mcp`), opened by `GET /mcp`.
    mcp: Option<Arc<sun_context::McpSessions>>,
    auth: Arc<auth::AuthState>,
    /// Host the design-canvas previews run on (`with_preview_host`).
    preview_host: Option<HostId>,
    previews: preview::Previews,
    /// The operator allows `bypass_permissions` (`sun.yaml` `allow_bypass`).
    allow_bypass: bool,
    /// Where runtime reads come from (`sun.yaml` `runtimes.model`).
    runtimes_model: RuntimesModel,
    /// Sun Cloud is connected, so `credits` runtimes can be added (`with_sun_cloud`).
    sun_cloud: bool,
    /// Lists and installs bundled and marketplace mods (`with_mod_installer`).
    mod_installer: Option<Arc<dyn ModInstaller>>,
    /// Each workspace's plan (`with_entitlements`); Community by default.
    entitlements: Arc<dyn sun_core::entitlement::EntitlementProvider>,
    /// Sun Cloud's marketplace (`with_marketplace`).
    market: Option<Arc<dyn Marketplace>>,
    /// Sun Cloud's billing, proxied (`with_cloud`).
    cloud: Option<Arc<billing::CloudBilling>>,
    /// This kernel's public URL, which leased seller hosts join.
    public_url: Option<String>,
    /// Built-in features that run (`with_features`); all by default.
    features: Features,
    /// Sends invites and the SMTP test (`with_mailer`).
    mailer: Option<Arc<dyn Mailer>>,
    /// The kernel URL invite mails link to (`with_mailer`).
    mail_url: Option<String>,
    /// Who may sign up and create workspaces (`with_signup_policy`).
    signup: Arc<sun_core::model::SignupPolicy>,
    /// How often host connections are pinged, and the silence after which
    /// one is dropped (`with_host_liveness`).
    host_liveness: (std::time::Duration, std::time::Duration),
}

impl Api {
    /// Bundles the engine, the queue, the host hub (also given to the engine
    /// as its `HostLink`) and the settings.
    pub fn new(engine: Engine, queue: Queue, hosts: HostHub, config: ApiConfig) -> Self {
        Self {
            engine,
            queue,
            hosts,
            token: Arc::new(std::sync::RwLock::new(config.token.clone())),
            rotate_token: None,
            config: Arc::new(config),
            catalog: None,
            mods: None,
            mcp: None,
            auth: Arc::default(),
            preview_host: None,
            previews: Default::default(),
            allow_bypass: false,
            runtimes_model: RuntimesModel::default(),
            sun_cloud: false,
            mod_installer: None,
            entitlements: Arc::new(sun_core::entitlement::Community),
            market: None,
            cloud: None,
            public_url: None,
            features: Features::default(),
            mailer: None,
            mail_url: None,
            signup: Arc::default(),
            host_liveness: (hosts::PING_EVERY, hosts::LIVENESS),
        }
    }

    /// Who may sign up (`SUN_ALLOW_SIGNUP`, `SUN_ALLOWED_EMAIL_DOMAINS`) and
    /// create workspaces (`SUN_DISABLE_WORKSPACE_CREATION`); by default
    /// anyone may both.
    pub fn with_signup_policy(mut self, policy: sun_core::model::SignupPolicy) -> Self {
        self.signup = Arc::new(policy);
        self
    }

    /// Rotates the kernel token through `rotate` when the first-run owner
    /// replaces the password that was the token (it was printed, maybe into
    /// service logs): the old token stops working at once. Without it the
    /// token never changes.
    pub fn with_token_rotation(mut self, rotate: TokenRotation) -> Self {
        self.rotate_token = Some(rotate);
        self
    }

    /// The kernel token now.
    fn token(&self) -> String {
        self.token
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// Sends invite and test mails through `mailer`, linking to the kernel
    /// at `public_url`. Without it those routes answer 503.
    pub fn with_mailer(mut self, mailer: Arc<dyn Mailer>, public_url: &str) -> Self {
        self.mailer = Some(mailer);
        self.mail_url = Some(public_url.trim_end_matches('/').to_owned());
        self
    }

    /// Signs in with the Google and GitHub presets at `urls` (tests' mock).
    pub fn with_preset_urls(mut self, urls: PresetUrls) -> Self {
        let mut auth = auth::AuthState::default();
        auth.sso.urls = urls;
        self.auth = Arc::new(auth);
        self
    }

    /// Answers 404 on the routes of every feature `features` turns off.
    pub fn with_features(mut self, features: Features) -> Self {
        self.features = features;
        self
    }

    /// Serves `/marketplace/...` through `market` (Sun Cloud); leased seller
    /// hosts join this kernel at `public_url`. Without it those routes answer 503.
    pub fn with_marketplace(mut self, market: Arc<dyn Marketplace>, public_url: &str) -> Self {
        self.market = Some(market);
        self.public_url = Some(public_url.to_owned());
        self
    }

    /// Proxies `/subscriptions/...`, `/billing/...` and `/support` to Sun
    /// Cloud through `cloud`, which also may ask `/internal/...`; turns on
    /// the `billing` and `cloud_hosts` features. Without it those routes
    /// answer 503 `billing_unavailable`.
    pub fn with_cloud(mut self, cloud: billing::CloudBilling) -> Self {
        self.cloud = Some(Arc::new(cloud));
        self
    }

    /// Serves `GET /mcp` (the `sun mcp` shim's transport) for the tokens the
    /// engine issues from `sessions`. Without it `/mcp` answers 503.
    pub fn with_mcp(mut self, sessions: Arc<sun_context::McpSessions>) -> Self {
        self.mcp = Some(sessions);
        self
    }

    /// Forwards `POST /api/v1/webhooks/{mod}` to `mods` (the engine's
    /// `ModLink`). Without it webhooks answer 503.
    pub fn with_mods(mut self, mods: Arc<dyn ModLink>) -> Self {
        self.mods = Some(mods);
        self
    }

    /// Runs design-canvas previews on `host`: the engine's
    /// `EngineConfig::host`, where the task worktrees live. Without it the
    /// preview routes answer 503.
    pub fn with_preview_host(mut self, host: HostId) -> Self {
        self.preview_host = Some(host);
        self
    }

    /// Pings host connections every `ping_every` and drops one silent for
    /// `liveness` (default 10 s / 30 s); tests shorten both.
    pub fn with_host_liveness(
        mut self,
        ping_every: std::time::Duration,
        liveness: std::time::Duration,
    ) -> Self {
        self.host_liveness = (ping_every, liveness);
        self
    }

    /// Lets `PUT /runtimes/{id}/policy` set `bypass_permissions` (the
    /// operator's `allow_bypass`); refused otherwise.
    pub fn with_allow_bypass(mut self, allow: bool) -> Self {
        self.allow_bypass = allow;
        self
    }

    /// Reads runtimes per `model` (`sun.yaml` `runtimes.model`); `v1` by
    /// default, see [`RuntimesModel`].
    pub fn with_runtimes_model(mut self, model: RuntimesModel) -> Self {
        self.runtimes_model = model;
        self
    }

    /// Sun Cloud is connected (`SUN_CLOUD_URL` and `SUN_CLOUD_TOKEN`):
    /// `POST /runtimes` may add `credits` runtimes, refused otherwise.
    pub fn with_sun_cloud(mut self, connected: bool) -> Self {
        self.sun_cloud = connected;
        self
    }

    /// Lists `installer`'s packages in `GET /mods` (those not installed yet)
    /// and serves `POST /mods/{name}/install`; without it that route answers 503.
    pub fn with_mod_installer(mut self, installer: Arc<dyn ModInstaller>) -> Self {
        self.mod_installer = Some(installer);
        self
    }

    /// Takes each workspace's plan (`GET /entitlement`, plan limits) from
    /// `provider`. Without it every workspace is Community, unlimited.
    pub fn with_entitlements(
        mut self,
        provider: Arc<dyn sun_core::entitlement::EntitlementProvider>,
    ) -> Self {
        self.entitlements = provider;
        self
    }

    /// Serves `/catalog` from `catalog`; without it those routes answer 503.
    pub fn with_catalog(mut self, catalog: sun_catalog::Catalog) -> Self {
        self.catalog = Some(Arc::new(catalog));
        self
    }
}

/// The whole HTTP surface: `/api/v1/**` (token required, except
/// `POST /api/v1/session`), `GET /host` (hosts authenticate in-band),
/// `/preview/<token>/…` (design-canvas previews; the token is the credential)
/// and the embedded web UI at `/` (public static files; its data needs the token).
pub fn router(api: Api) -> Router {
    let guard = axum::middleware::from_fn_with_state(api.clone(), features::guard);
    hosts::mark_started();
    let routed = routes::router(api.clone())
        .merge(hosts::router(api.clone()))
        .merge(mcp::router(api.clone()))
        .merge(preview::router(api.clone()))
        // Liveness for supervisors (Sun Cloud): the store is open.
        .route("/healthz", axum::routing::get(|| async { "ok" }))
        .fallback(ui::serve)
        .layer(guard);
    // Task keys in paths become ids before routing.
    let keys = axum::middleware::from_fn_with_state(api, tasks::key_paths);
    // An outer router of nothing but a fallback: its layer runs before the
    // inner one routes.
    Router::new().fallback_service(routed).layer(keys)
}
