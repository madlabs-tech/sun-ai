//! A runtime adapter an extension serves (`provides.runtime_adapters`, mod
//! API 1.2): **Adapter** from [`RuntimeAdapter`] to the mod's JSON-RPC
//! `runtime.*` actions over [`ModLink`], so a mod runtime joins failover
//! chains like a built-in one.
//!
//! The mod never gets a secret: `start` sends the spec without its
//! `env`. Events are polled (`runtime.events` from an index) and redacted
//! here like every adapter's.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use sun_core::{
    BoxStream, Error, HostCtx, ModCall, ModLink, Readiness, Result, RuntimeAdapter, SessionHandle,
    SessionId, SessionSpec, Steer, StopMode, SunEvent, Transcript, runtime_action,
};
use tokio::sync::mpsc;

use crate::process::{redact_event, stream};

/// How long to wait before asking again when a poll brought no event.
const POLL_EVERY: Duration = Duration::from_millis(200);

/// Runtime `<mod>.<adapter>`, served by installed extension `mod_name`.
pub struct ModRuntimeAdapter {
    id: String,
    mod_name: String,
    adapter: String,
    link: Arc<dyn ModLink>,
}

impl ModRuntimeAdapter {
    /// The adapter `adapter` that mod `mod_name` declared, reached over `link`.
    pub fn new(link: Arc<dyn ModLink>, mod_name: &str, adapter: &str) -> Self {
        Self {
            id: format!("{mod_name}.{adapter}"),
            mod_name: mod_name.into(),
            adapter: adapter.into(),
            link,
        }
    }

    async fn call<T: DeserializeOwned>(&self, action: &str, params: Value) -> Result<T> {
        call(&*self.link, &self.mod_name, &self.adapter, action, params).await
    }
}

async fn call<T: DeserializeOwned>(
    link: &dyn ModLink,
    mod_name: &str,
    adapter: &str,
    action: &str,
    mut params: Value,
) -> Result<T> {
    params["adapter"] = json!(adapter);
    let reply = link
        .call(ModCall {
            mod_name: mod_name.into(),
            method: action.into(),
            params,
            idem_key: None,
        })
        .await?;
    serde_json::from_value(reply)
        .map_err(|e| Error::Protocol(format!("{mod_name}.{adapter}: {action}: {e}")))
}

/// The `runtime.events` reply.
#[derive(serde::Deserialize)]
struct Events {
    events: Vec<SunEvent>,
}

/// The `runtime.start` reply.
#[derive(serde::Deserialize)]
struct Started {
    #[serde(default)]
    native_id: Option<String>,
}

#[async_trait]
impl RuntimeAdapter for ModRuntimeAdapter {
    fn id(&self) -> &str {
        &self.id
    }

    async fn readiness(&self, host: &HostCtx) -> Readiness {
        let params = json!({ "host_id": host.host_id });
        match self.call(runtime_action::READINESS, params).await {
            Ok(readiness) => readiness,
            Err(e) => Readiness {
                installed: false,
                version: None,
                logged_in: false,
                detail: Some(format!("extension {}: {e}", self.mod_name)),
            },
        }
    }

    async fn start(&self, mut spec: SessionSpec) -> Result<SessionHandle> {
        spec.env.clear();
        let session_id = spec.session_id;
        let started: Started = self
            .call(runtime_action::START, json!({ "spec": spec }))
            .await?;
        Ok(SessionHandle {
            session_id,
            native_id: started.native_id,
            pid: None,
        })
    }

    async fn send(&self, session: &SessionHandle, msg: Steer) -> Result<()> {
        let params = json!({ "session_id": session.session_id, "text": msg.text });
        self.call::<Value>(runtime_action::SEND, params)
            .await
            .map(drop)
    }

    async fn stop(&self, session: &SessionHandle, how: StopMode) -> Result<()> {
        let params = json!({ "session_id": session.session_id, "how": how });
        self.call::<Value>(runtime_action::STOP, params)
            .await
            .map(drop)
    }

    fn events(&self, session: &SessionHandle) -> BoxStream<'static, SunEvent> {
        let (tx, rx) = mpsc::unbounded_channel();
        let (link, mod_name, adapter) = (
            self.link.clone(),
            self.mod_name.clone(),
            self.adapter.clone(),
        );
        let session_id = session.session_id;
        tokio::spawn(async move {
            let mut after = 0;
            while !tx.is_closed() {
                let polled = poll(&*link, &mod_name, &adapter, session_id, after).await;
                let events = polled.unwrap_or_else(|e| {
                    let message = format!("extension {mod_name}: {e}");
                    vec![SunEvent::Error { message }, SunEvent::Exited { code: None }]
                });
                if events.is_empty() {
                    tokio::time::sleep(POLL_EVERY).await;
                    continue;
                }
                after += events.len();
                for event in events {
                    let exited = matches!(event, SunEvent::Exited { .. });
                    if tx.send(event).is_err() || exited {
                        return;
                    }
                }
            }
        });
        stream(Some(rx))
    }

    async fn export(&self, session: &SessionHandle) -> Result<Transcript> {
        let link = &*self.link;
        let events = poll(link, &self.mod_name, &self.adapter, session.session_id, 0).await?;
        Ok(Transcript {
            events,
            native: None,
        })
    }

    fn context_window(&self, _model: &str) -> Option<u32> {
        None
    }
}

/// The session's events from index `after` on, redacted.
async fn poll(
    link: &dyn ModLink,
    mod_name: &str,
    adapter: &str,
    session_id: SessionId,
    after: usize,
) -> Result<Vec<SunEvent>> {
    let params = json!({ "session_id": session_id, "after": after });
    let mut reply: Events = call(link, mod_name, adapter, runtime_action::EVENTS, params).await?;
    for event in &mut reply.events {
        redact_event(event, &[]);
    }
    Ok(reply.events)
}
