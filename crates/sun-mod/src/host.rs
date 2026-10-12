//! [`ModHost`]: the registry of installed mods and their process lifecycle
//! (lazy start, restart with backoff, idle stop).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::{Arc, RwLock, Weak};
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use serde_json::Value;
use sun_core::{BoxStream, Error, ModCall, ModEvent, ModLink, Result};
use sun_mod_sdk::{
    API_VERSION, CallParams, Deprecation, InitParams, Manifest, ModKind, PluginCallParams,
    RpcError, SubscribeParams, code, method,
};
use tokio::sync::{Mutex, mpsc};

use crate::manifest::{check_manifest, load_manifest};
use crate::process::{ModProcess, OnRequest};
use crate::sandbox::Sandbox;

/// Timeouts and backoff of the mod lifecycle.
#[derive(Debug, Clone)]
pub struct HostConfig {
    /// Longest a request (`init`, `call`, ...) may take; then the mod is killed.
    pub call_timeout: Duration,
    /// A mod with no calls and no subscriptions for this long is stopped.
    pub idle_timeout: Duration,
    /// First restart delay after a crash; doubles per crash.
    pub backoff_base: Duration,
    /// Restart delay cap. A process that lived longer than this resets the backoff.
    pub backoff_max: Duration,
    /// How long `shutdown` may take before the mod is killed.
    pub shutdown_grace: Duration,
    /// Mod → kernel methods that still work but log a warning on each use.
    pub deprecated: &'static [Deprecation],
}

impl Default for HostConfig {
    fn default() -> Self {
        Self {
            call_timeout: Duration::from_secs(60),
            idle_timeout: Duration::from_secs(300),
            backoff_base: Duration::from_millis(500),
            backoff_max: Duration::from_secs(30),
            shutdown_grace: Duration::from_secs(2),
            deprecated: sun_mod_sdk::DEPRECATED,
        }
    }
}

/// The **Plugin** host: implements [`ModLink`] over mod subprocesses.
///
/// Contract: a mod starts on first use, a crash fails only the calls in
/// flight on it, the next use restarts it after a backoff, mods with
/// subscriptions are restarted (and re-subscribed) eagerly, and idle mods
/// without subscriptions are stopped. An extension may call the plugins it
/// declares in `permissions.plugins` (`plugin_call`), and nothing else.
/// Needs a Tokio runtime.
pub struct ModHost {
    mods: Arc<Registry>,
    shared: Arc<Shared>,
}

type Registry = RwLock<HashMap<String, Arc<Slot>>>;

struct Shared {
    config: HostConfig,
    listeners: std::sync::Mutex<Vec<mpsc::UnboundedSender<ModEvent>>>,
}

struct Slot {
    manifest: Manifest,
    dir: PathBuf,
    sandbox: Option<Sandbox>,
    /// Sent as `init.config`: the install's non-secret settings.
    config: Value,
    shared: Arc<Shared>,
    /// The other mods, for `plugin_call` (weak: the registry owns the slots).
    registry: Weak<Registry>,
    state: Mutex<SlotState>,
}

#[derive(Default)]
struct SlotState {
    process: Option<Arc<ModProcess>>,
    crashes: u32,
    retry_at: Option<Instant>,
    triggers: Vec<String>,
    last_used: Option<Instant>,
    removed: bool,
}

impl ModHost {
    /// An empty host.
    pub fn new(config: HostConfig) -> Self {
        Self {
            mods: Arc::default(),
            shared: Arc::new(Shared {
                config,
                listeners: std::sync::Mutex::default(),
            }),
        }
    }

    /// Installs a mod from its folder (`dir/mod.yaml`); returns its name.
    pub async fn add_dir(&self, dir: &Path) -> Result<String> {
        let manifest = load_manifest(dir)?;
        let name = manifest.name.clone();
        self.add(manifest, dir.to_path_buf()).await?;
        Ok(name)
    }

    /// Installs a mod whose entry command is relative to `dir`. A mod with
    /// the same name is replaced (stopped first). Nothing starts yet.
    pub async fn add(&self, manifest: Manifest, dir: PathBuf) -> Result<()> {
        self.insert(manifest, dir, None, Value::Null).await
    }

    /// Like [`add`](Self::add), but the mod runs inside `sandbox` (third-party
    /// and private mods). Build it with [`Sandbox::for_mod`].
    pub async fn add_sandboxed(
        &self,
        manifest: Manifest,
        dir: PathBuf,
        sandbox: Sandbox,
    ) -> Result<()> {
        self.insert(manifest, dir, Some(sandbox), Value::Null).await
    }

    /// Installs a mod (sandboxed when `sandbox` is given) whose `init` carries
    /// `config`: the install's non-secret settings, e.g. a self-hosted
    /// `api_url`. Secrets never go here; they reach a sandboxed mod as env vars.
    pub async fn add_configured(
        &self,
        manifest: Manifest,
        dir: PathBuf,
        sandbox: Option<Sandbox>,
        config: Value,
    ) -> Result<()> {
        self.insert(manifest, dir, sandbox, config).await
    }

    async fn insert(
        &self,
        manifest: Manifest,
        dir: PathBuf,
        sandbox: Option<Sandbox>,
        config: Value,
    ) -> Result<()> {
        check_manifest(&manifest)?;
        let slot = Arc::new(Slot {
            manifest,
            dir,
            sandbox,
            config,
            shared: self.shared.clone(),
            registry: Arc::downgrade(&self.mods),
            state: Mutex::default(),
        });
        let old = write(&self.mods).insert(slot.manifest.name.clone(), slot);
        if let Some(old) = old {
            old.retire().await;
        }
        Ok(())
    }

    /// Stops and uninstalls a mod.
    pub async fn remove(&self, name: &str) -> Result<()> {
        let slot = write(&self.mods).remove(name);
        slot.ok_or_else(|| not_installed(name))?.retire().await;
        Ok(())
    }

    /// Calls `health` on a mod, starting it if needed.
    pub async fn health(&self, name: &str) -> Result<Value> {
        let slot = self.slot(name)?;
        slot.request(method::HEALTH, Value::Null).await
    }

    /// Whether the mod's process is running right now.
    pub async fn is_running(&self, name: &str) -> bool {
        match self.slot(name) {
            Ok(slot) => slot
                .state
                .lock()
                .await
                .process
                .as_ref()
                .is_some_and(|p| p.is_alive()),
            Err(_) => false,
        }
    }

    /// Stops every mod (kernel shutdown).
    pub async fn shutdown(&self) {
        let slots: Vec<_> = write(&self.mods).drain().map(|(_, s)| s).collect();
        for slot in slots {
            slot.retire().await;
        }
    }

    fn slot(&self, name: &str) -> Result<Arc<Slot>> {
        slot_in(&self.mods, name)
    }
}

fn slot_in(registry: &Registry, name: &str) -> Result<Arc<Slot>> {
    let mods = registry.read().unwrap_or_else(|p| p.into_inner());
    mods.get(name).cloned().ok_or_else(|| not_installed(name))
}

#[async_trait]
impl ModLink for ModHost {
    async fn call(&self, call: ModCall) -> Result<Value> {
        let slot = self.slot(&call.mod_name)?;
        slot.call(CallParams {
            action: call.method,
            params: call.params,
            idem_key: call.idem_key.map(|k| k.as_str().to_owned()),
        })
        .await
    }

    async fn subscribe(&self, mod_name: &str, triggers: Vec<String>) -> Result<()> {
        let slot = self.slot(mod_name)?;
        let provided = &slot.manifest.provides.triggers;
        if let Some(unknown) = triggers.iter().find(|t| !provided.contains(t)) {
            return Err(Error::InvalidInput(format!(
                "mod {mod_name} does not provide trigger {unknown}"
            )));
        }
        // Several kernel features subscribe the same mod: the mod gets the
        // union (the protocol's `subscribe` always carries the full set).
        // From now on the supervisor restarts the mod on a crash without
        // waiting for a call, and never idle-stops it.
        let triggers = {
            let mut state = slot.state.lock().await;
            for t in triggers {
                if !state.triggers.contains(&t) {
                    state.triggers.push(t);
                }
            }
            state.triggers.clone()
        };
        let (process, started) = slot.ensure().await?;
        if !started {
            // A fresh process already got these triggers from `start`.
            let params = to_json(SubscribeParams { triggers })?;
            let timeout = slot.shared.config.call_timeout;
            process.request(method::SUBSCRIBE, params, timeout).await?;
        }
        Ok(())
    }

    fn events(&self) -> BoxStream<'static, ModEvent> {
        let (tx, rx) = mpsc::unbounded_channel();
        lock(&self.shared.listeners).push(tx);
        Box::pin(Events(rx))
    }
}

impl Slot {
    /// `call` for an action, step type or gate the manifest provides.
    async fn call(self: &Arc<Self>, call: CallParams) -> Result<Value> {
        if !self.manifest.is_callable(&call.action) {
            return Err(Error::InvalidInput(format!(
                "mod {} does not provide {}",
                self.manifest.name, call.action
            )));
        }
        self.request(method::CALL, to_json(call)?).await
    }

    /// Answers this mod's requests (API 1.1): a deprecated method warns and
    /// still works; `plugin_call` reaches only the extension's declared plugins.
    fn on_request(&self) -> OnRequest {
        let caller = Arc::new(self.manifest.clone());
        let registry = self.registry.clone();
        let deprecated = self.shared.config.deprecated;
        Box::new(move |name, params| {
            let (caller, registry) = (caller.clone(), registry.clone());
            Box::pin(async move {
                if let Some(d) = deprecated.iter().find(|d| d.method == name) {
                    tracing::warn!(
                        target: "mod",
                        "mod {}: {} is deprecated since mod API {}; use {}",
                        caller.name, d.method, d.since, d.use_instead
                    );
                }
                match name.as_str() {
                    method::PLUGIN_CALL => plugin_call(&registry, &caller, params).await,
                    other => Err(RpcError::new(
                        code::METHOD_NOT_FOUND,
                        format!("unknown method {other}"),
                    )),
                }
            })
        })
    }

    async fn request(self: &Arc<Self>, method: &str, params: Value) -> Result<Value> {
        let (process, _) = self.ensure().await?;
        process
            .request(method, params, self.shared.config.call_timeout)
            .await
    }

    /// The running process, started (and supervised) if needed; `true` when
    /// this call started it.
    async fn ensure(self: &Arc<Self>) -> Result<(Arc<ModProcess>, bool)> {
        let (process, started) = self.running().await?;
        if started {
            tokio::spawn(supervise(Arc::downgrade(self), process.clone()));
        }
        Ok((process, started))
    }

    /// The running process, started after any backoff if needed; `true` when
    /// this call started it (the caller then owns its supervision).
    async fn running(&self) -> Result<(Arc<ModProcess>, bool)> {
        let mut st = self.state.lock().await;
        if st.removed {
            return Err(not_installed(&self.manifest.name));
        }
        st.last_used = Some(Instant::now());
        if let Some(process) = st.process.clone() {
            if process.is_alive() {
                return Ok((process, false));
            }
            self.crashed(&mut st, &process);
        }
        if let Some(at) = st.retry_at {
            tokio::time::sleep_until(at.into()).await;
        }
        let process = match self.start(&st.triggers).await {
            Ok(process) => Arc::new(process),
            Err(e) => {
                st.crashes += 1;
                st.retry_at = Some(Instant::now() + self.backoff(st.crashes));
                return Err(e);
            }
        };
        st.process = Some(process.clone());
        Ok((process, true))
    }

    /// After `crashed` exited: restarts the mod at once if it has
    /// subscriptions. Returns the new process to supervise, if this call
    /// started one. `running` sleeps the backoff, so retries are rate-bound.
    async fn restart_after_crash(&self, crashed: &Arc<ModProcess>) -> Option<Arc<ModProcess>> {
        {
            let mut st = self.state.lock().await;
            self.crashed(&mut st, crashed);
            if st.triggers.is_empty() || st.removed || st.process.is_some() {
                return None;
            }
        }
        loop {
            match self.running().await {
                Ok((process, started)) => return started.then_some(process),
                Err(_) => {
                    let st = self.state.lock().await;
                    if st.removed || st.triggers.is_empty() {
                        return None;
                    }
                }
            }
        }
    }

    /// Spawns the mod, sends `init` and re-sends the current subscription.
    async fn start(&self, triggers: &[String]) -> Result<ModProcess> {
        let name = self.manifest.name.clone();
        let provided = self.manifest.provides.triggers.clone();
        let shared = Arc::downgrade(&self.shared);
        let process = ModProcess::spawn(
            &self.manifest,
            &self.dir,
            self.sandbox.as_ref(),
            move |event| {
                // Validate at the boundary: only declared triggers reach the engine.
                if let (true, Some(shared)) = (provided.contains(&event.topic), shared.upgrade()) {
                    shared.publish(ModEvent {
                        mod_name: name.clone(),
                        topic: event.topic,
                        payload: event.payload,
                    });
                }
            },
            self.on_request(),
        )?;
        let timeout = self.shared.config.call_timeout;
        let init = to_json(InitParams {
            api_version: API_VERSION.into(),
            mod_name: self.manifest.name.clone(),
            config: self.config.clone(),
        })?;
        process.request(method::INIT, init, timeout).await?;
        if !triggers.is_empty() {
            let params = to_json(SubscribeParams {
                triggers: triggers.to_vec(),
            })?;
            process.request(method::SUBSCRIBE, params, timeout).await?;
        }
        Ok(process)
    }

    /// Records the crash of `process` if it is still the current one.
    fn crashed(&self, st: &mut SlotState, process: &Arc<ModProcess>) {
        if !st.process.as_ref().is_some_and(|p| Arc::ptr_eq(p, process)) {
            return;
        }
        st.process = None;
        let config = &self.shared.config;
        st.crashes = if process.started_at.elapsed() > config.backoff_max {
            1
        } else {
            st.crashes + 1
        };
        st.retry_at = Some(Instant::now() + self.backoff(st.crashes));
    }

    fn backoff(&self, crashes: u32) -> Duration {
        let config = &self.shared.config;
        let factor = 2u32.saturating_pow(crashes.saturating_sub(1));
        config
            .backoff_base
            .saturating_mul(factor)
            .min(config.backoff_max)
    }

    async fn retire(&self) {
        let process = {
            let mut st = self.state.lock().await;
            st.removed = true;
            st.process.take()
        };
        if let Some(process) = process {
            process.stop(self.shared.config.shutdown_grace).await;
        }
    }
}

/// Watches the mod's process: records a crash (restarting the mod at once
/// when it has subscriptions) or stops it once idle.
async fn supervise(slot: Weak<Slot>, mut process: Arc<ModProcess>) {
    loop {
        let Some(idle) = slot.upgrade().map(|s| s.shared.config.idle_timeout) else {
            return;
        };
        let exited = tokio::select! {
            () = process.exited() => true,
            () = tokio::time::sleep(idle) => false,
        };
        let Some(slot) = slot.upgrade() else { return };
        if exited {
            match slot.restart_after_crash(&process).await {
                Some(next) => process = next,
                None => return,
            }
            continue;
        }
        let mut st = slot.state.lock().await;
        if !st
            .process
            .as_ref()
            .is_some_and(|p| Arc::ptr_eq(p, &process))
        {
            return;
        }
        let idle_for = st.last_used.map_or(idle, |t| t.elapsed());
        if st.triggers.is_empty() && idle_for >= idle && process.in_flight() == 0 {
            st.process = None;
            drop(st);
            process.stop(slot.shared.config.shutdown_grace).await;
            return;
        }
    }
}

impl Shared {
    fn publish(&self, event: ModEvent) {
        // ponytail: unbounded per listener; bound it if a slow consumer ever
        // shows up (the engine drains into the store).
        lock(&self.listeners).retain(|tx| tx.send(event.clone()).is_ok());
    }
}

struct Events(mpsc::UnboundedReceiver<ModEvent>);

impl futures_core::Stream for Events {
    type Item = ModEvent;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<ModEvent>> {
        self.0.poll_recv(cx)
    }
}

/// An extension's `plugin_call`: only to a plugin it declares.
async fn plugin_call(
    registry: &Weak<Registry>,
    caller: &Manifest,
    params: Value,
) -> std::result::Result<Value, RpcError> {
    let call: PluginCallParams =
        serde_json::from_value(params).map_err(|e| RpcError::invalid(e.to_string()))?;
    if caller.kind != ModKind::Extension || !caller.permissions.plugins.contains(&call.plugin) {
        return Err(RpcError::invalid(format!(
            "mod {} may not call plugin {}: an extension must declare it in permissions.plugins",
            caller.name, call.plugin
        )));
    }
    let slot = registry
        .upgrade()
        .ok_or_else(|| Error::Unavailable("the kernel is shutting down".into()))
        .and_then(|r| slot_in(&r, &call.plugin))
        .and_then(|slot| match slot.manifest.kind {
            ModKind::Plugin => Ok(slot),
            ModKind::Extension => Err(not_installed(&call.plugin)),
        })
        .map_err(rpc_error)?;
    let outcome = slot.call(CallParams {
        action: call.action,
        params: call.params,
        idem_key: call.idem_key,
    });
    outcome.await.map_err(rpc_error)
}

/// A kernel error as the mod sees it; only `Unavailable` is retryable.
fn rpc_error(e: Error) -> RpcError {
    match e {
        Error::Unavailable(m) => RpcError::unavailable(m),
        Error::NotFound(m) => RpcError::new(code::METHOD_NOT_FOUND, m),
        refused @ Error::Refused { http_status, .. } => {
            RpcError::failed(refused.to_string()).with_http_status(http_status)
        }
        other => RpcError::failed(other.to_string()),
    }
}

fn to_json(value: impl serde::Serialize) -> Result<Value> {
    serde_json::to_value(value).map_err(|e| Error::InvalidInput(e.to_string()))
}

fn not_installed(name: &str) -> Error {
    Error::NotFound(format!("mod {name} is not installed"))
}

fn write<T>(m: &RwLock<T>) -> std::sync::RwLockWriteGuard<'_, T> {
    m.write().unwrap_or_else(|p| p.into_inner())
}

fn lock<T>(m: &std::sync::Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}
