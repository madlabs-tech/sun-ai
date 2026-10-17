//! `sun serve [--with-host]`: boots the kernel (store, mods, engine, queue,
//! API, web UI) and, with `--with-host`, a runtime host in the same process
//! that dials the kernel over loopback WebSocket — the same code path as a
//! remote `sun host`.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use sun_api::{Api, ApiConfig, HostHub};
use sun_core::entitlement::EntitlementProvider;
use sun_core::model::Host;
use sun_core::{HostId, RuntimeAdapter, WorkspaceId};
use sun_engine::{
    Attention, Caps, DecisionClient, Engine, EngineConfig, McpSessions, Principal, Queue,
    QueueConfig, SessionMcp, Slot, WatchdogConfig,
};
use sun_mod::ModHost;
use sun_store::{HostRepo, SecretRepo, SettingRepo, Store, WorkspaceItem, WorkspaceRepo};

use crate::cloud::{CLOUD_TOKEN_ENV, CLOUD_URL_ENV};
use crate::config::{Config, KernelSecrets, write_private};
use crate::installer::KernelInstaller;
use crate::mods;

/// How `sun serve` runs.
#[derive(Debug, Clone)]
pub struct ServeOptions {
    /// The data dir (`sun.db`, `kernel.json`, `host/`, `mods/`).
    pub data_dir: PathBuf,
    /// Also run a runtime host in this process.
    pub with_host: bool,
    /// Overrides `sun.yaml`'s `listen` (e.g. `127.0.0.1:0` in tests).
    pub listen: Option<String>,
    /// Run installed mods in the OS sandbox (default). Off: dev and tests.
    pub sandbox_mods: bool,
    /// The `sun` binary agent sessions run as their `sun` MCP server
    /// (`sun mcp`): `current_exe()`, or the built binary in tests.
    pub sun_exe: PathBuf,
    /// The in-process host's container/VM tools (`--with-host`).
    pub exec_env_tools: ExecEnvTools,
}

/// The exec env flags of `sun host run` and `sun serve --with-host`.
/// Unset: [`sun_host::HostConfig::new`]'s defaults.
#[derive(Debug, Clone, Default, clap::Args)]
pub struct ExecEnvTools {
    /// Container CLI for `exec_env: container` (`docker` or `podman`). Default: docker.
    #[arg(long)]
    pub container_cli: Option<PathBuf>,
    /// Lima CLI for `exec_env: vm`. Default: limactl.
    #[arg(long)]
    pub vm_cli: Option<PathBuf>,
    /// Seconds a container/VM stays up after its session for the gates. Default: 600.
    #[arg(long)]
    pub env_linger: Option<u64>,
    /// Never boot simulators or emulators here (`devices.enabled: false`,
    /// the VPS profile): device verify then needs another host.
    #[arg(long)]
    pub no_devices: bool,
}

impl ExecEnvTools {
    /// `config` with the flags that were given.
    pub fn apply(self, mut config: sun_host::HostConfig) -> sun_host::HostConfig {
        if let Some(cli) = self.container_cli {
            config.container_cli = cli;
        }
        if let Some(cli) = self.vm_cli {
            config.vm_cli = cli;
        }
        if let Some(secs) = self.env_linger {
            config.env_linger = Duration::from_secs(secs);
        }
        config.devices.enabled = !self.no_devices;
        config
    }
}

/// A booted kernel.
pub struct Kernel {
    /// Where the API and UI listen.
    pub addr: SocketAddr,
    /// The local admin token.
    pub token: String,
    /// The UI's public URL.
    pub url: String,
    /// The email the owner signs in with, the kernel token being the
    /// password, when this boot seeded it (first run, or an owner from
    /// without a password); `None` otherwise. Their first own
    /// password rotates the token in `kernel.json`.
    pub first_admin: Option<String>,
    /// Ends with the first fatal error of the engine, queue or server.
    pub done: tokio::task::JoinHandle<Result<()>>,
}

/// The local host's working dir and credentials.
fn host_dir(data_dir: &Path) -> PathBuf {
    data_dir.join("host")
}

/// The agent CLIs this box can run, configured from `sun.yaml`.
pub fn adapters(config: &Config) -> Vec<Arc<dyn RuntimeAdapter>> {
    let policy = &config.runtimes;
    let mut claude = sun_runtime::ClaudeCode::default();
    claude.permission_mode = policy.permission_mode;
    claude.allowed_tools = policy.allowed_tools.clone();
    claude.template_dir = policy.template_dir.clone();
    let mut codex = sun_runtime::Codex::default();
    codex.template_dir = policy.template_dir.clone();
    let mut opencode = sun_runtime::OpenCode::default();
    opencode.permission_mode = policy.permission_mode;
    opencode.allow_shell = policy.allowed_tools.iter().any(|t| t.starts_with("Bash"));
    opencode.template_dir = policy.template_dir.clone();
    let mut out: Vec<Arc<dyn RuntimeAdapter>> =
        vec![Arc::new(claude), Arc::new(codex), Arc::new(opencode)];
    for (id, fake) in &config.runtimes.fake {
        out.push(Arc::new(sun_runtime::FakeRuntime::new(
            id,
            &fake.binary,
            &fake.script,
        )));
    }
    out
}

/// Each runtime's context window for its CLI's default model (sessions run
/// with `model: None`); the engine rotates sessions near it.
fn context_windows(config: &Config) -> std::collections::HashMap<String, u32> {
    adapters(config)
        .iter()
        .filter_map(|a| Some((a.id().to_owned(), a.context_window("default")?)))
        .collect()
}

/// What loaded extensions add to the in-process host (mod API 1.2): their
/// runtime adapters (`<mod>.<id>`) and exec envs (`<mod>.<kind>`).
// ponytail: read at boot; an extension added later serves runtimes and envs
// after a restart. Re-open the host on install when that matters.
fn extension_runtimes(
    loaded: &[sun_mod::Manifest],
    mods: &Arc<ModHost>,
) -> (Vec<Arc<dyn RuntimeAdapter>>, sun_host::ModEnvs) {
    let link: Arc<dyn sun_core::ModLink> = mods.clone();
    let extensions = loaded
        .iter()
        .filter(|m| m.kind == sun_mod::ModKind::Extension);
    let mut adapters: Vec<Arc<dyn RuntimeAdapter>> = Vec::new();
    let mut names = Vec::new();
    for m in extensions {
        for id in &m.provides.runtime_adapters {
            let adapter = sun_runtime::ModRuntimeAdapter::new(link.clone(), &m.name, id);
            adapters.push(Arc::new(adapter));
        }
        names.extend(
            m.provides
                .exec_envs
                .iter()
                .map(|k| format!("{}.{k}", m.name)),
        );
    }
    (adapters, sun_host::ModEnvs { link, names })
}

/// Boots the kernel; `adapters` are the local host's runtimes (`--with-host`).
pub async fn start(
    options: ServeOptions,
    mut adapters: Vec<Arc<dyn RuntimeAdapter>>,
) -> Result<Kernel> {
    let data_dir = &options.data_dir;
    std::fs::create_dir_all(data_dir)
        .with_context(|| format!("creating data dir {}", data_dir.display()))?;
    let config = Config::load(data_dir)?;
    let secrets = KernelSecrets::load_or_create(data_dir)?;
    let store = Store::open(data_dir)
        .await
        .with_context(|| format!("opening the store in {}", data_dir.display()))?;
    sun_core::redact::set_known_secrets(store.read().await?.known_secrets().await?);
    let mut tx = store.begin().await?;
    let retention = serde_json::json!(config.worktree_retention_hours.unwrap_or(0));
    tx.put_setting(sun_engine::WORKTREE_RETENTION, &retention)
        .await?;
    let model = serde_json::json!(config.runtimes.model);
    tx.put_setting(sun_engine::RUNTIMES_MODEL, &model).await?;
    tx.commit().await?;
    let host = placement_host(&store, data_dir, &secrets, options.with_host).await?;

    let mods = Arc::new(ModHost::new(sun_mod::HostConfig::default()));
    let loaded =
        mods::load_installed(&store, data_dir, &config, &mods, options.sandbox_mods).await?;
    let names: Vec<String> = loaded.iter().map(|m| m.name.clone()).collect();
    if !names.is_empty() {
        tracing::info!("mods: {}", names.join(", "));
    }

    // Bound first: sessions' `sun mcp` shims dial the public URL.
    let listener =
        tokio::net::TcpListener::bind(options.listen.as_deref().unwrap_or(config.listen()))
            .await
            .with_context(|| format!("listening on {}", config.listen()))?;
    let addr = listener.local_addr()?;
    write_address(data_dir, addr)?;
    let public_url = config
        .public_url
        .clone()
        .unwrap_or_else(|| format!("http://{addr}"));
    let public_url = public_url.trim_end_matches('/').to_owned();

    let runtime = config
        .default_runtime
        .clone()
        .unwrap_or_else(|| sun_runtime::claude::ID.into());
    let hub = HostHub::default();
    let features = config.features;
    let mcp_sessions = McpSessions::new();
    let mcp_sessions = Arc::new(match features.knowledge {
        true => mcp_sessions.with_knowledge(store.clone()),
        false => mcp_sessions,
    });
    // Under Sun Cloud, `credits` runtimes get per-session gateway keys.
    let hosts: Arc<dyn sun_core::HostLink> = Arc::new(hub.clone());
    crate::cloud::seed_credits_runtimes(&store, &config).await?;
    let keys = crate::cloud::SessionKeys::from_env(|name| std::env::var(name).ok());
    let sun_cloud = keys.is_some();
    let hosts = match keys {
        Some(keys) => Arc::new(crate::cloud::CreditsLink::new(hosts, store.clone(), keys)),
        None => hosts,
    };
    let _sidecar = sidecar(&config)?;
    // Under Sun Cloud, tasks can be leased on marketplace runtimes;
    // the engine's outbox reaches the marketplace as a pseudo-mod.
    let market = crate::cloud::CloudClient::from_env(|name| std::env::var(name).ok())
        .filter(|_| features.market)
        .map(|client| -> Arc<dyn sun_core::Marketplace> {
            Arc::new(crate::market::CloudMarket::new(client))
        });
    let engine_mods: Arc<dyn sun_core::ModLink> = match &market {
        Some(market) => Arc::new(crate::market::MarketMods::new(mods.clone(), market.clone())),
        None => mods.clone(),
    };
    let (engine, engine_task) = Engine::start_with(
        store.clone(),
        hosts,
        engine_mods,
        EngineConfig {
            host,
            adapter: runtime.clone(),
            delivery_timeout: Duration::from_secs(30),
            watchdog: WatchdogConfig {
                stall_after: config
                    .watchdog
                    .stall_after_secs()?
                    .map_or(WatchdogConfig::default().stall_after, Duration::from_secs),
                on_stall: config.watchdog.on_stall.unwrap_or_default(),
                context_windows: context_windows(&config),
                rotate_at_percent: config
                    .rotate_at_percent
                    .unwrap_or(WatchdogConfig::default().rotate_at_percent),
                ..WatchdogConfig::default()
            },
        },
        decisions(&config, mods.clone())?,
        Some(SessionMcp {
            sessions: mcp_sessions.clone(),
            command: "sun".into(),
            local: options
                .with_host
                .then(|| (host, options.sun_exe.display().to_string())),
            args: vec!["mcp".into(), "--kernel".into(), public_url.clone()],
        }),
    )
    .await?;
    // Mod triggers: workflow `on:` events, merges,
    // reviews and alerts; the mod host keeps each mod's union.
    engine.subscribe_triggers(&names).await?;
    let merges = tokio::spawn(engine.clone().follow_merges());
    // Run hooks: plugins with an `observe` action and extensions with
    // `hooks`/`metrics` follow the event log.
    let observers: Vec<_> = loaded
        .iter()
        .filter_map(|m| Some((m.name.clone(), mods::hooks_of(m)?)))
        .map(|(name, hooks)| tokio::spawn(engine.clone().feed_observer(name, hooks)))
        .collect();
    // Repo-scope memory proposals become PRs.
    let memory = tokio::spawn(engine.clone().follow_memory());
    // The decision engine labels new corrections by cause.
    let classifier = config
        .decisions
        .is_some()
        .then(|| tokio::spawn(engine.clone().follow_corrections()));
    // Eval replays don't survive a restart.
    if features.evals {
        engine.fail_interrupted_evals().await?;
    }
    // Retention: finished runs' timelines compacted after N days.
    let retention = config
        .event_retention_days
        .map(|days| tokio::spawn(engine.clone().follow_retention(days)));
    // Approvals and stuck steps reach people with a deep link.
    let notify = (!config.notify_to.is_empty()).then(|| Attention {
        public_url: public_url.clone(),
        to: config.notify_to.clone(),
    });
    let attention = notify
        .clone()
        .map(|to| tokio::spawn(engine.clone().follow_attention(to)));
    if engine.workflows().await?.is_empty() {
        engine
            .save_workflow(None, sun_workflow::DEFAULT_WORKFLOW)
            .await?;
    }
    let (queue, queue_task) = Queue::start(
        store.clone(),
        engine.clone(),
        QueueConfig {
            slot: Slot { runtime, host },
            caps: Caps {
                per_ecosystem: config.max_active_runs,
                per_runtime: config.caps.per_runtime.clone(),
                per_host: config.caps.per_host.clone(),
                max_sessions: config.runtimes.max_sessions,
            },
        },
    );

    let api_config = ApiConfig {
        token: secrets.token.clone(),
        host_secret: secrets.host_secret.clone(),
        principal: Principal {
            user: "admin".into(),
            roles: vec!["admin".into()],
        },
        public_url: public_url.clone(),
    };
    import_oidc(&config, &engine).await?;
    let catalog = sun_catalog::Catalog::open(data_dir)?;
    let skills_dir = catalog.skills_dir();
    let sync_every = Duration::from_secs(60 * config.knowledge_sync_minutes.unwrap_or(15).max(1));
    let knowledge_task = features
        .knowledge
        .then(|| engine.start_knowledge_worker(mods.clone(), sync_every));
    let installer = KernelInstaller {
        store: store.clone(),
        data_dir: data_dir.clone(),
        host: mods.clone(),
        sandbox: options.sandbox_mods,
        trusted_keys: trusted_keys()?,
        registry: crate::registry::entries().to_vec(),
        engine: Some(engine.clone()),
        loaded: Default::default(),
    };
    // `sun plugin|extension add|update|remove` apply without a restart.
    installer.seed().await?;
    let installer = Arc::new(installer);
    tokio::spawn(installer.clone().follow(Duration::from_secs(2)));
    let triggers = tokio::spawn(engine.clone().follow_triggers(notify.clone()));
    // `cron:` workflow triggers.
    let cron = tokio::spawn(
        engine
            .clone()
            .follow_cron(notify, config.cron_catch_up.unwrap_or(true)),
    );
    // A crash between a lease hold and Sun Cloud's answer must not strand a task.
    engine.clear_lease_holds().await?;
    if let Some(market) = &market {
        tokio::spawn(engine.clone().follow_leases(market.clone()));
    }
    let api = Api::new(engine, queue, hub, api_config);
    let api = match market {
        Some(market) => {
            let api = api.with_marketplace(market, &public_url);
            tokio::spawn(api.clone().follow_market_failover());
            api
        }
        None => api,
    };
    if config.registry_url.is_some() {
        tracing::warn!("sun.yaml `registry_url` is ignored: plans come from Sun Cloud only");
    }
    let api = match (std::env::var(CLOUD_URL_ENV), std::env::var(CLOUD_TOKEN_ENV)) {
        (Ok(url), Ok(token)) => api.with_cloud(sun_api::CloudBilling::new(&url, &token)?),
        _ => api,
    };
    let entitlements = entitlements(|name| std::env::var(name).ok())?;
    let api = api
        .with_mods(mods.clone())
        .with_preview_host(host)
        .with_features(features)
        .with_entitlements(entitlements)
        .with_allow_bypass(config.runtimes.allow_bypass)
        .with_runtimes_model(config.runtimes.model)
        .with_sun_cloud(sun_cloud)
        .with_mod_installer(installer)
        .with_mcp(mcp_sessions)
        .with_mailer(Arc::new(crate::mail::SmtpMailer), &public_url)
        .with_signup_policy(sun_core::model::SignupPolicy::from_env(|name| {
            std::env::var(name).ok()
        }))
        .with_token_rotation({
            let data_dir = data_dir.clone();
            Arc::new(move || KernelSecrets::rotate_token(&data_dir).map_err(|e| format!("{e:#}")))
        });
    let api = match features.catalog {
        true => api.with_catalog(catalog),
        false => api,
    };
    let first_admin = api.seed_owner().await?;
    let app = sun_api::router(api);
    // The client address feeds the sign-in rate limit (per email and IP).
    let app = app.into_make_service_with_connect_info::<std::net::SocketAddr>();
    let server = tokio::spawn(async move { axum::serve(listener, app).await });

    let host_task = match options.with_host {
        true => {
            let (runtimes, mod_envs) = extension_runtimes(&loaded, &mods);
            adapters.extend(runtimes);
            let config = options.exec_env_tools.clone().apply(sun_host::HostConfig {
                skills_dir,
                mod_envs: Some(mod_envs),
                permission_mode: Some(config.runtimes.permission_mode),
                ..sun_host::HostConfig::new(host_dir(data_dir))
            });
            let local = sun_host::Host::open(config, adapters)?;
            let mut credentials = sun_host::Credentials::load(&host_dir(data_dir))?;
            credentials.kernel_url = format!("ws://{addr}/host");
            Some(tokio::spawn(local.run(credentials)))
        }
        false => None,
    };

    let done = tokio::spawn(async move {
        let result = tokio::select! {
            r = engine_task => r.context("engine task").and_then(|r| r.context("engine stopped")),
            r = queue_task => r.context("queue task").and_then(|r| r.context("queue stopped")),
            r = server => r.context("server task").and_then(|r| r.context("server stopped")),
        };
        if let Some(host) = host_task {
            host.abort();
        }
        merges.abort();
        triggers.abort();
        if let Some(classifier) = classifier {
            classifier.abort();
        }
        cron.abort();
        observers.iter().for_each(tokio::task::JoinHandle::abort);
        memory.abort();
        if let Some(attention) = attention {
            attention.abort();
        }
        knowledge_task
            .iter()
            .for_each(tokio::task::JoinHandle::abort);
        retention.iter().for_each(tokio::task::JoinHandle::abort);
        mods.shutdown().await;
        result
    });
    Ok(Kernel {
        addr,
        url: public_url,
        first_admin,
        token: secrets.token,
        done,
    })
}

/// Where `sun serve` reports the address it bound (`--port 0` picks one).
pub const ADDRESS_FILE: &str = "address";

/// `<data_dir>/address`: the bound `host:port`, written atomically (temp +
/// rename) so a reader (Sun Cloud's driver, `sun status`) never sees half of it.
fn write_address(data_dir: &Path, addr: SocketAddr) -> Result<()> {
    let path = data_dir.join(ADDRESS_FILE);
    let temp = data_dir.join("address.tmp");
    std::fs::write(&temp, addr.to_string())
        .and_then(|()| std::fs::rename(&temp, &path))
        .with_context(|| format!("writing {}", path.display()))
}

/// The fallback host: the queue places each run on a ready online host and
/// uses this one only while no host has reported the runtime yet.
/// `--with-host`: the local host, enrolled on first boot by writing its
/// credentials (`host/host.json`) directly. Otherwise the oldest enrolled host.
async fn placement_host(
    store: &Store,
    data_dir: &Path,
    secrets: &KernelSecrets,
    with_host: bool,
) -> Result<HostId> {
    let dir = host_dir(data_dir);
    if let Ok(credentials) = sun_host::Credentials::load(&dir) {
        let known = store
            .read()
            .await?
            .get_host(credentials.host_id)
            .await
            .is_ok();
        if known
            && credentials.host_key
                == sun_api::host_key_for(&secrets.host_secret, credentials.host_id)
        {
            return Ok(credentials.host_id);
        }
    }
    if !with_host {
        if let Some(first) = store.read().await?.hosts().await?.first() {
            return Ok(first.id);
        }
        tracing::warn!(
            "no runtime host yet: add one (`sun host join`) and restart, or use --with-host"
        );
        return Ok(HostId::generate());
    }
    let host = Host {
        id: HostId::generate(),
        name: "local".into(),
        capabilities: serde_json::json!({}),
        enrolled_by: None,
    };
    let mut tx = store.begin().await?;
    tx.insert_host(&host).await?;
    // ponytail: the kernel's own host takes Main's runs only; a deployment
    // owner attaches it to other workspaces (`PUT /workspaces/{id}/hosts/{host}`).
    tx.link(WorkspaceId::MAIN, WorkspaceItem::Host(host.id))
        .await?;
    tx.commit().await?;
    let credentials = sun_host::Credentials {
        kernel_url: String::new(),
        host_id: host.id,
        host_key: sun_api::host_key_for(&secrets.host_secret, host.id),
    };
    write_private(
        &dir.join("host.json"),
        &serde_json::to_vec_pretty(&credentials)?,
    )?;
    Ok(host.id)
}

/// `SUN_TRUSTED_KEYS`: comma-separated hex keys a signed bundled package
/// must match when installed from the UI (as `sun plugin add --trust`).
fn trusted_keys() -> Result<Vec<[u8; 32]>> {
    let keys = std::env::var("SUN_TRUSTED_KEYS").unwrap_or_default();
    keys.split(',')
        .filter(|k| !k.trim().is_empty())
        .map(|k| mods::parse_key(k).context("SUN_TRUSTED_KEYS"))
        .collect()
}

/// `sun.yaml`'s `decisions`: a URL (plus `SUN_DECISIONS_API_KEY`, never in
/// the file) or a plugin mod.
fn decisions(config: &Config, mods: Arc<ModHost>) -> Result<Option<DecisionClient>> {
    let Some(d) = &config.decisions else {
        return Ok(None);
    };
    match (&d.url, &d.plugin) {
        (Some(url), None) => {
            let key = std::env::var("SUN_DECISIONS_API_KEY").ok();
            let client = DecisionClient::new(url, key).context("sun.yaml decisions.url")?;
            Ok(Some(client))
        }
        (None, Some(name)) => Ok(Some(DecisionClient::plugin(mods, name))),
        _ => anyhow::bail!("sun.yaml decisions: set one of `url` and `plugin`"),
    }
}

/// The decision sidecar process (Laya), stopped when `sun serve` ends.
struct Sidecar(std::process::Child);

impl Drop for Sidecar {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Starts `decisions.sidecar` (e.g. `[laya, serve, --port, "8090"]`) beside
/// the kernel: a separate process, never inside the kernel binary.
// ponytail: launched on the kernel's box (its local host); a remote host's
// sidecar is started there by its operator and named by `url`.
fn sidecar(config: &Config) -> Result<Option<Sidecar>> {
    let command = config.decisions.as_ref().map(|d| d.sidecar.as_slice());
    let Some((program, args)) = command.and_then(<[String]>::split_first) else {
        return Ok(None);
    };
    let child = std::process::Command::new(program)
        .args(args)
        .stdin(std::process::Stdio::null())
        .spawn()
        .with_context(|| format!("starting the decision sidecar `{program}`"))?;
    Ok(Some(Sidecar(child)))
}

/// Imports `sun.yaml`'s old `oidc:` entry (plus `SUN_OIDC_CLIENT_SECRET`)
/// into Settings → Sign-in as provider `oidc`, once; after that the
/// entry is ignored. Its callback moves to `/api/v1/auth/sso/oidc/callback`.
async fn import_oidc(config: &Config, engine: &Engine) -> Result<()> {
    let Some(oidc) = &config.oidc else {
        return Ok(());
    };
    let provider = sun_core::sso::SsoProvider {
        id: "oidc".into(),
        kind: sun_core::sso::SsoKind::Oidc,
        name: oidc.name.clone(),
        client_id: oidc.client_id.clone(),
        issuer_url: Some(oidc.issuer_url.clone()),
        allowed_domains: oidc.allowed_domains.clone(),
    };
    let secret = std::env::var("SUN_OIDC_CLIENT_SECRET").ok();
    let imported = engine
        .import_sso_provider(provider, secret.as_deref())
        .await
        .context("importing sun.yaml `oidc:` into Settings → Sign-in")?;
    if imported {
        tracing::warn!(
            "imported sun.yaml `oidc:` into Settings → Sign-in; register the callback \
             /api/v1/auth/sso/oidc/callback at the provider and remove `oidc:` from sun.yaml"
        );
    } else {
        tracing::warn!("sun.yaml `oidc:` is ignored: sign-in providers live in Settings → Sign-in");
    }
    Ok(())
}

/// Each workspace's plan: Sun Cloud's policy under Sun Cloud
/// (`SUN_CLOUD_URL` and `SUN_CLOUD_TOKEN`), else Community.
fn entitlements(get: impl Fn(&str) -> Option<String>) -> Result<Arc<dyn EntitlementProvider>> {
    let source = entitlement_source(&get);
    tracing::info!("plans: {source}");
    match (get(CLOUD_URL_ENV), get(CLOUD_TOKEN_ENV)) {
        (Some(url), Some(token)) => Ok(Arc::new(sun_api::CloudPolicy::new(&url, &token)?)),
        _ => Ok(Arc::new(sun_core::entitlement::Community)),
    }
}

/// Where the deployment's plans come from: `cloud` or `community`.
pub fn entitlement_source(get: impl Fn(&str) -> Option<String>) -> &'static str {
    match get(CLOUD_URL_ENV).is_some() && get(CLOUD_TOKEN_ENV).is_some() {
        true => "cloud",
        false => "community",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plans_come_from_the_cloud_only_with_its_url_and_token() {
        let only = |key: &'static str| move |name: &str| (name == key).then(|| "x".to_owned());
        assert_eq!(entitlement_source(|_| Some("x".into())), "cloud");
        assert_eq!(entitlement_source(only(CLOUD_URL_ENV)), "community");
        assert_eq!(entitlement_source(only(CLOUD_TOKEN_ENV)), "community");
        assert_eq!(entitlement_source(|_| None), "community");
    }

    #[test]
    fn decision_sidecar_runs_until_serve_ends_and_a_plugin_needs_no_url() {
        let load = |yaml: &str| {
            let dir = tempfile::tempdir().unwrap();
            std::fs::write(dir.path().join("sun.yaml"), yaml).unwrap();
            Config::load(dir.path()).unwrap()
        };
        let config = load("decisions: {url: http://127.0.0.1:1, sidecar: [sleep, '30']}");
        let sidecar = sidecar(&config).unwrap().unwrap();
        let pid = sidecar.0.id().to_string();
        let alive = || {
            std::process::Command::new("kill")
                .args(["-0", &pid])
                .status()
                .unwrap()
                .success()
        };
        assert!(alive());
        drop(sidecar);
        assert!(!alive(), "stopped with serve");

        let mods = Arc::new(ModHost::new(sun_mod::HostConfig::default()));
        let plugin = load("decisions: {plugin: jev}");
        let client = decisions(&plugin, mods.clone()).unwrap().unwrap();
        assert_eq!(client.url(), None);
        let both = load("decisions: {url: http://x, plugin: jev}");
        assert!(decisions(&both, mods).is_err());
    }
}
