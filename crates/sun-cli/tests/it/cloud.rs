//! Sun Cloud kernel side: `credits` runtimes get a per-session gateway key
//! from a mocked control plane, injected into the session env, and revoked
//! when the session exits or fails to start. Other runtimes pass through.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use axum::http::{HeaderMap, StatusCode};
use axum::routing::post;
use futures_util::StreamExt;
use sun_cli::cloud::{CreditsLink, SessionKeys};
use sun_core::model::{Budget, Host, Run, RuntimeSource, Step, Task, Workflow};
use sun_core::{
    BoxStream, Error, HostEvent, HostId, HostLink, Result, RunId, RunStatus, SessionHandle,
    SessionId, SessionSpec, Steer, StepId, StepKey, StepStatus, StopMode, SunEvent, TaskStatus,
    WorkflowId,
};
use sun_store::{HostRepo, RunRepo, RuntimeRepo, SessionRepo, Store, TaskRepo, WorkflowRepo};

type Calls = Arc<Mutex<Vec<(String, String, String)>>>;

/// The control plane's `/session-keys` endpoints; records `(path, auth, body)`.
async fn control_plane(calls: Calls) -> String {
    let record = |path: &'static str, calls: Calls| {
        move |headers: HeaderMap, body: String| async move {
            let auth = headers["authorization"].to_str().unwrap().to_owned();
            let mut calls = calls.lock().unwrap();
            calls.push((path.into(), auth, body));
            match path {
                "/session-keys" => (
                    StatusCode::CREATED,
                    format!(r#"{{"key":"sun_gk_{}"}}"#, calls.len()),
                ),
                _ => (StatusCode::NO_CONTENT, String::new()),
            }
        }
    };
    let app = axum::Router::new()
        .route(
            "/session-keys",
            post(record("/session-keys", calls.clone())),
        )
        .route(
            "/session-keys/revoke",
            post(record("/session-keys/revoke", calls)),
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    base
}

/// Records each start's adapter and env; fails starts of adapter `codex`.
#[derive(Default)]
struct Hosts {
    started: Mutex<Vec<(String, BTreeMap<String, String>)>>,
    events: Mutex<Vec<HostEvent>>,
}

#[async_trait]
impl HostLink for Hosts {
    async fn start_session(
        &self,
        _host: HostId,
        adapter: &str,
        spec: SessionSpec,
    ) -> Result<SessionHandle> {
        if adapter == "codex" {
            return Err(Error::Unavailable("host gone".into()));
        }
        self.started
            .lock()
            .unwrap()
            .push((adapter.into(), spec.env.clone()));
        Ok(SessionHandle {
            session_id: spec.session_id,
            native_id: None,
            pid: None,
        })
    }
    async fn steer(&self, _session: &SessionHandle, _msg: Steer) -> Result<()> {
        Ok(())
    }
    async fn stop_session(&self, _session: &SessionHandle, _how: StopMode) -> Result<()> {
        Ok(())
    }
    async fn is_alive(&self, _session: &SessionHandle) -> Result<bool> {
        Ok(true)
    }
    fn events(&self) -> BoxStream<'static, HostEvent> {
        Box::pin(futures_util::stream::iter(std::mem::take(
            &mut *self.events.lock().unwrap(),
        )))
    }
    async fn ack(&self, _session: SessionId, _seq: u64) -> Result<()> {
        Ok(())
    }
}

fn spec() -> SessionSpec {
    SessionSpec {
        session_id: SessionId::generate(),
        run_id: RunId::generate(),
        step_key: StepKey::new("implement").unwrap(),
        worktree: PathBuf::from("/tmp"),
        envelope: String::new(),
        envelope_parts: None,
        model: None,
        mcp_config: None,
        env: BTreeMap::new(),
        exec_env: Default::default(),
        resume: None,
        permission_mode: None,
        byo_key: None,
    }
}

/// A run to hang agent steps on, as the engine has before a session starts.
async fn new_run(store: &Store) -> RunId {
    let workflow = Workflow {
        id: WorkflowId::generate(),
        workspace_id: sun_core::WorkspaceId::MAIN,
        name: "default".into(),
    };
    let task = Task {
        title: "t".into(),
        workflow_id: Some(workflow.id),
        status: TaskStatus::Todo,
        ..sun_core::model::Task::new(sun_core::WorkspaceId::MAIN, "")
    };
    let run = Run {
        id: RunId::generate(),
        task_id: task.id,
        workflow_id: workflow.id,
        workflow_version: 1,
        status: RunStatus::Queued,
        end_reason: None,
        budget: Budget::default(),
        repo_ids: Vec::new(),
    };
    let mut tx = store.begin().await.unwrap();
    tx.insert_workflow(&workflow).await.unwrap();
    tx.save_workflow_version(workflow.id, "name: default", "h")
        .await
        .unwrap();
    tx.insert_task(&task).await.unwrap();
    tx.insert_run(&run).await.unwrap();
    tx.commit().await.unwrap();
    run.id
}

/// What the engine does before `start_session`: a step attempt assigned to
/// runtime `name` and a new session. Returns that session's spec.
async fn assigned(store: &Store, run_id: RunId, attempt: u32, name: &str) -> SessionSpec {
    let spec = SessionSpec { run_id, ..spec() };
    let step = Step {
        id: StepId::generate(),
        run_id,
        step_key: spec.step_key.clone(),
        attempt,
        status: StepStatus::Queued,
        runtime_id: None,
        session_id: None,
        started_at: None,
        ended_at: None,
    };
    let mut tx = store.begin().await.unwrap();
    let runtime = tx.runtime_by_name(name).await.unwrap().unwrap();
    tx.insert_step(&step).await.unwrap();
    let host = Host {
        id: HostId::generate(),
        name: "box".into(),
        capabilities: serde_json::json!({}),
    };
    tx.insert_host(&host).await.unwrap();
    tx.insert_session(spec.session_id, host.id).await.unwrap();
    tx.assign_step_session(step.id, runtime.id, spec.session_id)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    spec
}

async fn wait_for(calls: &Calls, n: usize) {
    for _ in 0..100 {
        if calls.lock().unwrap().len() >= n {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("expected {n} calls, got {:?}", calls.lock().unwrap());
}

#[tokio::test]
async fn credits_sessions_get_a_gateway_key_revoked_on_exit() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).await.unwrap();
    let mut tx = store.begin().await.unwrap();
    for (name, adapter, source) in [
        ("claude-credits", "claude-code", RuntimeSource::Credits),
        ("codex-credits", "codex", RuntimeSource::Credits),
        ("opencode-credits", "opencode", RuntimeSource::Credits),
        ("claude-code", "claude-code", RuntimeSource::ByoRuntime),
    ] {
        tx.upsert_runtime(name, adapter, source).await.unwrap();
    }
    tx.commit().await.unwrap();
    let calls = Calls::default();
    let cloud = control_plane(calls.clone()).await;
    let hosts = Arc::new(Hosts::default());
    let run = new_run(&store).await;
    let link = CreditsLink::new(
        hosts.clone(),
        store.clone(),
        SessionKeys::from_env(|name| match name {
            "SUN_CLOUD_URL" => Some(format!("{cloud}/")),
            "SUN_CLOUD_TOKEN" => Some("t1.secret".into()),
            _ => None,
        })
        .unwrap(),
    );
    let host = HostId::generate();

    // A credits runtime: a key from the control plane, the CLI pointed at the
    // gateway, started on the runtime's adapter. The engine passes the
    // adapter id (`claude-code`), which the subscription runtime shares: the
    // runtime is found by the session's step, not by that id.
    let first = assigned(&store, run, 1, "claude-credits").await;
    link.start_session(host, "claude-code", first.clone())
        .await
        .unwrap();
    {
        let started = hosts.started.lock().unwrap();
        let (adapter, env) = &started[0];
        assert_eq!(adapter, "claude-code");
        assert_eq!(env["ANTHROPIC_BASE_URL"], cloud);
        assert_eq!(env["ANTHROPIC_API_KEY"], "sun_gk_1");
    }
    let (path, auth, _) = calls.lock().unwrap()[0].clone();
    assert_eq!(
        (path.as_str(), auth.as_str()),
        ("/session-keys", "Bearer t1.secret")
    );

    // Subscription runtimes and sessions without a step pass through untouched.
    let subscription = assigned(&store, run, 2, "claude-code").await;
    link.start_session(host, "claude-code", subscription)
        .await
        .unwrap();
    link.start_session(host, "fake", spec()).await.unwrap();
    assert!(
        hosts.started.lock().unwrap()[1..]
            .iter()
            .all(|(_, env)| env.is_empty())
    );
    assert_eq!(calls.lock().unwrap().len(), 1);

    // OpenCode without a gateway model: refused before any key is minted.
    let opencode = assigned(&store, run, 3, "opencode-credits").await;
    let refused = link.start_session(host, "opencode", opencode).await;
    assert!(matches!(refused, Err(Error::InvalidInput(_))));
    assert_eq!(calls.lock().unwrap().len(), 1);

    // A start that fails revokes its key.
    assert!(
        link.start_session(
            host,
            "codex",
            assigned(&store, run, 4, "codex-credits").await
        )
        .await
        .is_err()
    );
    wait_for(&calls, 3).await;
    assert_eq!(revoked(&calls, 2), "sun_gk_2");

    // The session exits: its key is revoked once.
    hosts.events.lock().unwrap().push(HostEvent {
        session_id: first.session_id,
        seq: 1,
        event: SunEvent::Exited { code: Some(0) },
    });
    let seen: Vec<HostEvent> = link.events().collect().await;
    assert_eq!(seen.len(), 1, "events pass through");
    wait_for(&calls, 4).await;
    assert_eq!(revoked(&calls, 3), "sun_gk_1");
}

/// The key that call `i` revoked.
fn revoked(calls: &Calls, i: usize) -> String {
    let (path, _, body) = calls.lock().unwrap()[i].clone();
    assert_eq!(path, "/session-keys/revoke");
    let body: serde_json::Value = serde_json::from_str(&body).unwrap();
    body["key"].as_str().unwrap().to_owned()
}

/// The engine's outbox row `sun_cloud.release_lease` reaches Sun
/// Cloud's (mocked) `POST /marketplace/v1/leases/{id}/release` with the
/// kernel token; reconcile releases again (idempotent); other mods pass by.
#[tokio::test]
async fn market_mods_release_a_lease_on_sun_cloud() {
    use axum::extract::Path;
    use sun_cli::market::{CloudMarket, MarketMods};
    use sun_core::{ModCall, ModLink};

    struct NoMods;
    #[async_trait]
    impl ModLink for NoMods {
        async fn call(&self, call: ModCall) -> Result<serde_json::Value> {
            Err(Error::NotFound(call.mod_name))
        }
    }

    let calls: Calls = Arc::default();
    let record = calls.clone();
    let release = move |Path(id): Path<String>, headers: HeaderMap| {
        let calls = record.clone();
        async move {
            let auth = headers["authorization"].to_str().unwrap().to_owned();
            calls
                .lock()
                .unwrap()
                .push((id.clone(), auth, String::new()));
            let body = serde_json::json!({ "id": id, "escrow": "refunded" });
            ([("content-type", "application/json")], body.to_string())
        }
    };
    let app = axum::Router::new().route("/marketplace/v1/leases/{id}/release", post(release));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

    let client = sun_cli::cloud::CloudClient::new(&base, "sun_kt_1");
    let mods = MarketMods::new(Arc::new(NoMods), Arc::new(CloudMarket::new(client)));
    let call = |method: &str, params: serde_json::Value| ModCall {
        mod_name: sun_engine::MARKET_MOD.into(),
        method: method.into(),
        params,
        idem_key: None,
    };
    let params = serde_json::json!({ "lease_id": "lea_1" });
    let released = mods
        .call(call(sun_engine::RELEASE_LEASE, params.clone()))
        .await
        .unwrap();
    assert_eq!(released["escrow"], "refunded");
    let reconcile = serde_json::json!({ "method": sun_engine::RELEASE_LEASE, "params": params });
    let reconciled = mods.call(call("reconcile", reconcile)).await.unwrap();
    assert_eq!(reconciled["delivered"], true);
    assert_eq!(reconciled["result"]["escrow"], "refunded");
    let bad = call(
        sun_engine::RELEASE_LEASE,
        serde_json::json!({ "lease_id": "../x" }),
    );
    assert!(matches!(mods.call(bad).await, Err(Error::InvalidInput(_))));
    let other = ModCall {
        mod_name: "github".into(),
        ..call("open_pr", serde_json::Value::Null)
    };
    assert!(matches!(mods.call(other).await, Err(Error::NotFound(_))));
    let calls = calls.lock().unwrap().clone();
    assert_eq!(calls.len(), 2, "{calls:?}");
    assert!(
        calls
            .iter()
            .all(|(id, auth, _)| id == "lea_1" && auth == "Bearer sun_kt_1")
    );
}
