//! A throwaway kernel for the web UI's Playwright run against the real API
//! (`web/tests/real.spec.ts`), until `sun serve` (1.16b) can stand in.
//!
//! Real store (temp dir), engine, queue and API, the embedded UI at `/`, token
//! `test-token`, and one approval-only workflow so a task needs no host.
//! `cargo run -p sun-api --example ui_server` (address: `SUN_UI_ADDR`, default
//! `127.0.0.1:7799`).
#![allow(clippy::unwrap_used, clippy::expect_used)] // test helper

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde_json::{Value, json};
use sun_api::{Api, ApiConfig, HostHub};
use sun_core::entitlement::{Entitlement, EntitlementProvider, EntitlementStatus, Plan};
use sun_core::model::Host;
use sun_core::{HostId, ModCall, ModLink, Result, WorkspaceId};
use sun_engine::{Engine, EngineConfig, Principal, Queue, QueueConfig, Slot, WatchdogConfig};
use sun_store::{HostRepo, Store, WorkspaceItem, WorkspaceRepo};

const WORKFLOW: &str = "
name: review-by-a-person
steps:
  - id: review
    type: approval
    who: [role:admin]
";

struct NoMods;

/// Every workspace on a Business plan from the cloud, so the plan badge
/// shows a paid plan.
struct Business;

#[async_trait]
impl EntitlementProvider for Business {
    async fn for_workspace(&self, _: WorkspaceId) -> Entitlement {
        Entitlement {
            plan: Plan::Business,
            paid: true,
            limits: Default::default(),
            status: EntitlementStatus::Cloud {
                valid_until: chrono::Utc::now() + chrono::Duration::minutes(5),
            },
        }
    }
}

#[async_trait]
impl ModLink for NoMods {
    async fn call(&self, _: ModCall) -> Result<Value> {
        Ok(json!({}))
    }
}

#[tokio::main]
async fn main() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).await.unwrap();
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

    let hosts = HostHub::default();
    let (engine, _) = Engine::start(
        store.clone(),
        Arc::new(hosts.clone()),
        Arc::new(NoMods),
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
    engine.save_workflow(None, WORKFLOW).await.unwrap();
    let (queue, _) = Queue::start(
        store,
        engine.clone(),
        QueueConfig {
            slot: Slot {
                runtime: "fake".into(),
                host,
            },
            caps: Default::default(),
        },
    );
    let addr = std::env::var("SUN_UI_ADDR").unwrap_or_else(|_| "127.0.0.1:7799".into());
    let config = ApiConfig {
        token: "test-token".into(),
        host_secret: "host-secret".into(),
        principal: Principal {
            user: "me".into(),
            roles: vec!["admin".into()],
        },
        // localhost, not 127.0.0.1: browsers allow passkeys there (passkey.real.spec).
        public_url: format!("http://{}", addr.replace("127.0.0.1", "localhost")),
    };
    let catalog = sun_catalog::Catalog::open(dir.path().join("data")).unwrap();
    let api = Api::new(engine.clone(), queue, hosts, config)
        .with_catalog(catalog)
        .with_entitlements(Arc::new(Business));
    // First run, as `sun serve` (admin@sun.local / the token); the owner
    // then gave their real email and kept the token as their own password
    // (no rotation here), so the UI opens on the workspace.
    api.seed_owner().await.unwrap();
    let owner = engine
        .set_owner_email("me", "me@example.com")
        .await
        .unwrap();
    let hash = engine.password_hash(owner.id).await.unwrap().unwrap();
    engine.set_own_password(owner.id, &hash).await.unwrap();
    let app = sun_api::router(api);
    let listener = tokio::net::TcpListener::bind(&addr).await.unwrap();
    println!("sun-api ui_server on http://{addr}/ (sign in as me@example.com / test-token)");
    axum::serve(listener, app).await.unwrap();
}
