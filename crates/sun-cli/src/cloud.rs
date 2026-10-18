//! Sun Cloud, kernel side of credits. A kernel run by
//! the Sun Cloud control plane gets [`CLOUD_URL_ENV`] and [`CLOUD_TOKEN_ENV`];
//! with both, every session paid by a `credits` credential gets a
//! short-lived gateway key from `POST /session-keys`, injected through
//! [`sun_runtime::credits::session_env`] and revoked when the session exits.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use futures_util::StreamExt;
use serde_json::{Value, json};
use sun_core::host_protocol::{
    DeviceFlowRequest, DeviceInput, DeviceRequest, DeviceResult, DeviceScreen, ExecOutput,
    ExecRequest, SessionFile, SessionStats, Worktree,
};
use sun_core::model::{Credential, RuntimeSource};
use sun_core::{
    BoxStream, CredentialId, Error, HostEvent, HostId, HostLink, Result, SessionHandle, SessionId,
    SessionSpec, Steer, StopMode, SunEvent,
};
use sun_store::{CredentialRepo, RunRepo, SettingRepo, Store};

/// The control plane's base URL (also the gateway's).
pub const CLOUD_URL_ENV: &str = "SUN_CLOUD_URL";
/// This kernel's control-plane credential, `<tenant id>.<secret>`.
pub const CLOUD_TOKEN_ENV: &str = "SUN_CLOUD_TOKEN";

/// HTTP client of Sun Cloud, authenticated with this kernel's token. Errors
/// carry Sun Cloud's message and status class, never the token.
pub struct CloudClient {
    base: String,
    token: String,
    agent: ureq::Agent,
}

impl CloudClient {
    /// A client of Sun Cloud at `base`, authenticated with `token`.
    pub fn new(base: &str, token: &str) -> Self {
        let agent = ureq::Agent::config_builder()
            .http_status_as_error(false)
            .timeout_global(Some(std::time::Duration::from_secs(30)))
            .build()
            .into();
        Self {
            base: base.trim_end_matches('/').to_owned(),
            token: token.to_owned(),
            agent,
        }
    }

    /// From [`CLOUD_URL_ENV`] and [`CLOUD_TOKEN_ENV`] (`get` is
    /// `std::env::var(..).ok()`); `None` unless both are set, i.e. outside Sun Cloud.
    pub fn from_env(get: impl Fn(&str) -> Option<String>) -> Option<Self> {
        Some(Self::new(&get(CLOUD_URL_ENV)?, &get(CLOUD_TOKEN_ENV)?))
    }

    /// Sun Cloud's base URL.
    pub fn base(&self) -> &str {
        &self.base
    }

    /// `GET path`, returning the JSON body.
    pub async fn get(self: &Arc<Self>, path: String) -> Result<Value> {
        self.request(path, None, false, json_body).await
    }

    /// `POST path` with `body` (or an empty body), returning the JSON body.
    pub async fn post(self: &Arc<Self>, path: String, body: Option<Value>) -> Result<Value> {
        self.request(path, body, true, json_body).await
    }

    /// `POST path` with an empty body, returning the raw body (a package),
    /// at most `limit` bytes.
    pub async fn post_bytes(self: &Arc<Self>, path: String, limit: u64) -> Result<Vec<u8>> {
        let label = path.clone();
        let read = move |body: &mut ureq::Body| {
            body.with_config()
                .limit(limit)
                .read_to_vec()
                .map_err(|e| Error::Unavailable(format!("Sun Cloud {label}: {e}")))
        };
        self.request(path, None, true, read).await
    }

    async fn request<T: Send + 'static>(
        self: &Arc<Self>,
        path: String,
        body: Option<Value>,
        post: bool,
        read: impl FnOnce(&mut ureq::Body) -> Result<T> + Send + 'static,
    ) -> Result<T> {
        let client = self.clone();
        let label = path.clone();
        let call = move || {
            let url = format!("{}{path}", client.base);
            let auth = format!("Bearer {}", client.token);
            let response = match (post, body) {
                (true, Some(body)) => client
                    .agent
                    .post(url)
                    .header("authorization", auth)
                    .send_json(body),
                (true, None) => client
                    .agent
                    .post(url)
                    .header("authorization", auth)
                    .send_empty(),
                (false, _) => client.agent.get(url).header("authorization", auth).call(),
            };
            // Never echo the token or a key: only the status and Sun Cloud's message.
            let mut response =
                response.map_err(|e| Error::Unavailable(format!("Sun Cloud {path}: {e}")))?;
            let status = response.status().as_u16();
            if (200..300).contains(&status) {
                return read(response.body_mut());
            }
            let body = response.body_mut().read_json().unwrap_or(Value::Null);
            let message = body["error"]
                .as_str()
                .map_or_else(|| format!("Sun Cloud {path}: {status}"), str::to_owned);
            Err(match status {
                400 | 409 | 422 => Error::InvalidInput(message),
                401..=403 => Error::Forbidden(message),
                404 => Error::NotFound(message),
                _ => Error::Unavailable(message),
            })
        };
        tokio::task::spawn_blocking(call)
            .await
            .map_err(|e| Error::Unavailable(format!("Sun Cloud {label}: {e}")))?
    }
}

/// A JSON body, `null` when there is none.
fn json_body(body: &mut ureq::Body) -> Result<Value> {
    Ok(body.read_json().unwrap_or(Value::Null))
}

/// Client of the control plane's session-key endpoints.
pub struct SessionKeys(Arc<CloudClient>);

impl SessionKeys {
    /// A client of the control plane at `base`, authenticated with `token`.
    pub fn new(base: &str, token: &str) -> Self {
        Self(Arc::new(CloudClient::new(base, token)))
    }

    /// From [`CLOUD_URL_ENV`] and [`CLOUD_TOKEN_ENV`]; `None` outside Sun Cloud.
    pub fn from_env(get: impl Fn(&str) -> Option<String>) -> Option<Self> {
        CloudClient::from_env(get).map(|c| Self(Arc::new(c)))
    }

    /// The gateway's base URL.
    pub fn gateway_url(&self) -> &str {
        self.0.base()
    }

    /// A new gateway key for one session.
    pub async fn issue(self: &Arc<Self>) -> Result<String> {
        let body = self.0.post("/session-keys".into(), None).await?;
        body["key"]
            .as_str()
            .map(str::to_owned)
            .ok_or_else(|| Error::Unavailable("Sun Cloud sent no session key".into()))
    }

    /// Revokes a key this kernel was issued.
    pub async fn revoke(self: &Arc<Self>, key: String) -> Result<()> {
        let body = json!({ "key": key });
        self.0
            .post("/session-keys/revoke".into(), Some(body))
            .await
            .map(drop)
    }
}

/// **Decorator** over the engine's [`HostLink`]: sessions paid by a
/// `credits` credential start with a fresh gateway key in their env (and on
/// the credential's tool); the key is revoked when the session exits or fails
/// to start. Everything else passes through unchanged.
// ponytail: live keys are kept in memory; after a kernel restart the keys of
// sessions still running are left to expire (SESSION_KEY_TTL on the cloud).
pub struct CreditsLink {
    inner: Arc<dyn HostLink>,
    store: Store,
    keys: Arc<SessionKeys>,
    live: Arc<Mutex<HashMap<SessionId, String>>>,
}

impl CreditsLink {
    /// Wraps `inner`; credentials are looked up in `store`.
    pub fn new(inner: Arc<dyn HostLink>, store: Store, keys: SessionKeys) -> Self {
        Self {
            inner,
            store,
            keys: Arc::new(keys),
            live: Arc::default(),
        }
    }
}

/// Creates a `credits` credential for each `sun.yaml` `runtimes.credits`
/// entry, so a workflow chain naming it resolves to Sun credits, and stores
/// its gateway model (OpenCode needs one) where credentials added from the
/// UI keep theirs. An existing credential keeps its settings (rows are never
/// rewritten here).
// ponytail: sun.yaml is the deployment's, so its credits credentials are
// Main's; other workspaces add their own (`POST /credentials`).
pub async fn seed_credits_runtimes(store: &Store, config: &crate::config::Config) -> Result<()> {
    let mut tx = store.begin().await?;
    for (name, credits) in &config.runtimes.credits {
        let seeded = Credential {
            id: CredentialId::generate(),
            workspace_id: sun_core::WorkspaceId::MAIN,
            name: name.clone(),
            source: RuntimeSource::Credits,
            default_tool: credits.adapter.clone(),
            byo_key: None,
            created_by: None,
        };
        tx.insert_credential(&seeded).await?;
        let stored = tx
            .credential_by_name(sun_core::WorkspaceId::MAIN, name)
            .await?
            .ok_or_else(|| Error::NotFound(format!("credential {name}")))?;
        if let Some(model) = &credits.model {
            tx.put_setting(&sun_engine::credits_model_key(stored.id), &json!(model))
                .await?;
        }
    }
    tx.commit().await?;
    Ok(())
}

/// Revokes `key` in the background; a failure only logs (the key expires anyway).
fn revoke_later(keys: Arc<SessionKeys>, key: String) {
    tokio::spawn(async move {
        if let Err(e) = keys.revoke(key).await {
            tracing::warn!("revoking a session key: {e}");
        }
    });
}

/// The credential the engine assigned this session to pay with (by name,
/// e.g. `claude-credits`), through the step that holds it. `start_session`
/// only gets the adapter id, which several credentials can share.
async fn session_credential(store: &Store, spec: &SessionSpec) -> Result<Option<Credential>> {
    let mut tx = store.read().await?;
    let step = tx
        .steps_of_run(spec.run_id)
        .await?
        .into_iter()
        .find(|s| s.session_id == Some(spec.session_id));
    match step.and_then(|s| s.credential_id) {
        Some(id) => Ok(Some(tx.get_credential(id).await?)),
        None => Ok(None),
    }
}

#[async_trait]
impl HostLink for CreditsLink {
    async fn start_session(
        &self,
        host: HostId,
        adapter: &str,
        mut spec: SessionSpec,
    ) -> Result<SessionHandle> {
        let credential = session_credential(&self.store, &spec).await?;
        let Some(credential) = credential.filter(|c| c.source == RuntimeSource::Credits) else {
            return self.inner.start_session(host, adapter, spec).await;
        };
        let model = self
            .store
            .read()
            .await?
            .setting(&sun_engine::credits_model_key(credential.id))
            .await?;
        let model = model.as_ref().and_then(Value::as_str);
        let env = |key: &str| {
            sun_runtime::credits::session_env(
                &credential.default_tool,
                self.keys.gateway_url(),
                key,
                model,
            )
        };
        env("")?; // Refuse an unsupported adapter before minting a key.
        let key = self.keys.issue().await?;
        spec.env.extend(env(&key)?);
        let session = spec.session_id;
        self.live
            .lock()
            .map_err(|_| Error::Unavailable("session keys lock poisoned".into()))?
            .insert(session, key.clone());
        let started = self
            .inner
            .start_session(host, &credential.default_tool, spec)
            .await;
        if started.is_err() {
            if let Ok(mut live) = self.live.lock() {
                live.remove(&session);
            }
            revoke_later(self.keys.clone(), key);
        }
        started
    }

    async fn steer(&self, session: &SessionHandle, msg: Steer) -> Result<()> {
        self.inner.steer(session, msg).await
    }

    async fn stop_session(&self, session: &SessionHandle, how: StopMode) -> Result<()> {
        self.inner.stop_session(session, how).await
    }

    async fn is_alive(&self, session: &SessionHandle) -> Result<bool> {
        self.inner.is_alive(session).await
    }

    fn events(&self) -> BoxStream<'static, HostEvent> {
        let (live, keys) = (self.live.clone(), self.keys.clone());
        Box::pin(self.inner.events().inspect(move |event| {
            if !matches!(event.event, SunEvent::Exited { .. }) {
                return;
            }
            let key = live
                .lock()
                .ok()
                .and_then(|mut l| l.remove(&event.session_id));
            if let Some(key) = key {
                revoke_later(keys.clone(), key);
            }
        }))
    }

    async fn ack(&self, session: SessionId, seq: u64) -> Result<()> {
        self.inner.ack(session, seq).await
    }

    fn is_online(&self, host: HostId) -> bool {
        self.inner.is_online(host)
    }

    async fn prepare_worktree(&self, host: HostId, worktree: Worktree) -> Result<PathBuf> {
        self.inner.prepare_worktree(host, worktree).await
    }

    async fn remove_worktree(&self, host: HostId, task_id: sun_core::TaskId) -> Result<()> {
        self.inner.remove_worktree(host, task_id).await
    }

    async fn write_files(&self, host: HostId, cwd: PathBuf, files: Vec<SessionFile>) -> Result<()> {
        self.inner.write_files(host, cwd, files).await
    }

    async fn exec(&self, host: HostId, request: ExecRequest) -> Result<ExecOutput> {
        self.inner.exec(host, request).await
    }

    async fn verify_on_device(&self, host: HostId, request: DeviceRequest) -> Result<DeviceResult> {
        self.inner.verify_on_device(host, request).await
    }

    async fn device_screen(&self, host: HostId, task_id: sun_core::TaskId) -> Result<DeviceScreen> {
        self.inner.device_screen(host, task_id).await
    }

    async fn device_input(
        &self,
        host: HostId,
        task_id: sun_core::TaskId,
        input: DeviceInput,
    ) -> Result<()> {
        self.inner.device_input(host, task_id, input).await
    }

    async fn device_flow(&self, host: HostId, request: DeviceFlowRequest) -> Result<DeviceResult> {
        self.inner.device_flow(host, request).await
    }

    async fn stats(&self, session: SessionId) -> Result<SessionStats> {
        self.inner.stats(session).await
    }
}
