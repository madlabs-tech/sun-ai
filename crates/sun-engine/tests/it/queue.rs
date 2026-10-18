//! Queue & flow control: priority then FIFO, caps, cooldown, task status.
//!
//! Runs are single `approval` steps, so a run holds its slot until the test
//! approves it — no timing games, no host.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicI64, Ordering};
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use serde_json::Value;
use sun_core::model::{Account, RuntimeSource, Task, Workflow};
use sun_core::{
    BoxStream, Error, HostEvent, HostId, HostLink, ModCall, ModLink, Result, RunId, RunStatus,
    SessionHandle, SessionId, SessionSpec, Steer, StepKey, StepStatus, StopMode, TaskId,
    TaskStatus, WorkflowId,
};
use sun_engine::{
    Caps, Engine, EngineConfig, HostCap, MarketPolicy, Principal, Queue, QueueConfig, Slot,
    TaskLease, WatchdogConfig,
};
use sun_store::{
    EventLog, NewEvent, RunRepo, RuntimeRepo, SettingRepo, Store, TaskRepo, WorkflowRepo,
};

const WORKFLOW: &str = r#"
name: gatekept
steps:
  - id: approve
    type: approval
    who: [role:admin]
"#;

pub(crate) struct NoHost;

#[async_trait]
impl HostLink for NoHost {
    async fn start_session(&self, _: HostId, _: &str, _: SessionSpec) -> Result<SessionHandle> {
        Err(Error::Unavailable("no host".into()))
    }
    async fn steer(&self, _: &SessionHandle, _: Steer) -> Result<()> {
        Ok(())
    }
    async fn stop_session(&self, _: &SessionHandle, _: StopMode) -> Result<()> {
        Ok(())
    }
    async fn is_alive(&self, _: &SessionHandle) -> Result<bool> {
        Ok(false)
    }
    fn events(&self) -> BoxStream<'static, HostEvent> {
        Box::pin(Ended)
    }
    async fn ack(&self, _: SessionId, _: u64) -> Result<()> {
        Ok(())
    }
}

pub(crate) struct NoMods;

#[async_trait]
impl ModLink for NoMods {
    async fn call(&self, _: ModCall) -> Result<Value> {
        Err(Error::Unavailable("no mods".into()))
    }
}

struct Ended;

impl futures_core::Stream for Ended {
    type Item = HostEvent;
    fn poll_next(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Option<HostEvent>> {
        Poll::Ready(None)
    }
}

async fn eventually<F, Fut>(what: &str, mut check: F)
where
    F: FnMut() -> Fut,
    Fut: Future<Output = bool>,
{
    let deadline = Instant::now() + Duration::from_secs(10);
    while !check().await {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

async fn run_of(store: &Store, task: TaskId) -> Option<(RunId, RunStatus)> {
    let mut tx = store.read().await.unwrap();
    let runs = tx.active_runs().await.unwrap();
    runs.iter()
        .find(|r| r.task_id == task)
        .map(|r| (r.id, r.status))
}

async fn task_status(store: &Store, task: TaskId) -> TaskStatus {
    store
        .read()
        .await
        .unwrap()
        .get_task(task)
        .await
        .unwrap()
        .status
}

/// The task's run started: it works, verifies or waits for a person (an
/// approval-only run waits at once).
async fn started(store: &Store, task: TaskId) -> bool {
    matches!(
        task_status(store, task).await,
        TaskStatus::InProgress | TaskStatus::Verifying | TaskStatus::NeedsYou
    )
}

/// Waits for `task`'s run to reach approval, approves it, waits for `done`.
async fn approve(store: &Store, engine: &Engine, task: TaskId) {
    let deadline = Instant::now() + Duration::from_secs(10);
    let run = loop {
        if let Some((id, RunStatus::Running)) = run_of(store, task).await {
            let steps = store.read().await.unwrap().steps_of_run(id).await.unwrap();
            if steps
                .iter()
                .any(|s| s.status == StepStatus::WaitingApproval)
            {
                break id;
            }
        }
        assert!(Instant::now() < deadline, "timed out waiting for approval");
        tokio::time::sleep(Duration::from_millis(10)).await;
    };
    let key = StepKey::new("approve").unwrap();
    engine
        .decide(
            run,
            key,
            true,
            Principal {
                user: "test".into(),
                roles: vec!["admin".into()],
            },
        )
        .await
        .unwrap();
    eventually("task done", || async {
        task_status(store, task).await == TaskStatus::Done
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn queue_respects_priority_fifo_caps_and_cooldown() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).await.unwrap();

    let parsed = sun_workflow::parse(WORKFLOW).unwrap();
    let workflow = Workflow {
        id: WorkflowId::generate(),
        workspace_id: sun_core::WorkspaceId::MAIN,
        name: parsed.name.clone(),
    };
    let tasks: Vec<Task> = [0, 0, 9]
        .into_iter()
        .map(|priority| Task {
            workflow_id: Some(workflow.id),
            priority: sun_core::model::Priority::from_int(priority),
            status: TaskStatus::Backlog,
            ..Task::new(sun_core::WorkspaceId::MAIN, format!("p{priority}"))
        })
        .collect();
    let mut tx = store.begin().await.unwrap();
    tx.insert_workflow(&workflow).await.unwrap();
    tx.save_workflow_version(workflow.id, WORKFLOW, &parsed.hash)
        .await
        .unwrap();
    for task in &tasks {
        tx.insert_task(task).await.unwrap();
    }
    tx.commit().await.unwrap();
    let (a, b, c) = (tasks[0].id, tasks[1].id, tasks[2].id);

    let host = HostId::generate();
    let (engine, _engine_task) = Engine::start(
        store.clone(),
        std::sync::Arc::new(NoHost),
        std::sync::Arc::new(NoMods),
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
    let (queue, _queue_task) = Queue::start(
        store.clone(),
        engine.clone(),
        QueueConfig {
            slot: Slot {
                runtime: "fake".into(),
                host,
            },
            caps: Caps {
                per_runtime: BTreeMap::from([("fake".into(), 1)]),
                ..Caps::default()
            },
        },
    );

    // `a` takes the only slot; `c` (priority 9) jumps ahead of `b`.
    queue.enqueue(a).await.unwrap();
    eventually("a running", || async { started(&store, a).await }).await;
    let run_b = queue.enqueue(b).await.unwrap();
    let run_c = queue.enqueue(c).await.unwrap();
    let order: Vec<RunId> = queue
        .waiting()
        .await
        .unwrap()
        .iter()
        .map(|r| r.id)
        .collect();
    assert_eq!(order, [run_c, run_b]);
    assert_eq!(task_status(&store, b).await, TaskStatus::Todo);
    assert!(
        queue.enqueue(b).await.is_err(),
        "a queued task can't queue twice"
    );

    approve(&store, &engine, a).await;
    // `c` runs next, and its runtime hits a limit: once `c` ends, `b` must
    // wait out the cooldown instead of taking the free slot.
    eventually("c running", || async { started(&store, c).await }).await;
    let reset_at = chrono::Utc::now() + chrono::Duration::milliseconds(1500);
    // What failover journals on a limit hit.
    let account = login(host, "fake");
    let cooling = serde_json::json!({ "runtime": "fake", "until": reset_at });
    let mut tx = store.begin().await.unwrap();
    tx.set_cooldown(&account, Some(reset_at)).await.unwrap();
    tx.append_event(NewEvent::new(run_c, None, "runtime_cooling", cooling))
        .await
        .unwrap();
    tx.commit().await.unwrap();
    approve(&store, &engine, c).await;
    assert!(
        queue.enqueue(a).await.is_err(),
        "a done task can't queue again"
    );
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(run_of(&store, b).await, Some((run_b, RunStatus::Queued)));

    approve(&store, &engine, b).await;
    assert!(
        chrono::Utc::now() >= reset_at,
        "b started before the cooldown ended"
    );
}

/// [`NoHost`] whose online hosts the test controls.
struct Online(std::sync::Mutex<Vec<HostId>>);

#[async_trait]
impl HostLink for Online {
    async fn start_session(&self, h: HostId, a: &str, s: SessionSpec) -> Result<SessionHandle> {
        NoHost.start_session(h, a, s).await
    }
    async fn steer(&self, _: &SessionHandle, _: Steer) -> Result<()> {
        Ok(())
    }
    async fn stop_session(&self, _: &SessionHandle, _: StopMode) -> Result<()> {
        Ok(())
    }
    async fn is_alive(&self, _: &SessionHandle) -> Result<bool> {
        Ok(false)
    }
    fn events(&self) -> BoxStream<'static, HostEvent> {
        Box::pin(Ended)
    }
    async fn ack(&self, _: SessionId, _: u64) -> Result<()> {
        Ok(())
    }
    fn is_online(&self, host: HostId) -> bool {
        self.0.lock().unwrap().contains(&host)
    }
}

async fn add_host(store: &Store, name: &str, logged_in: bool) -> HostId {
    use sun_store::{HostRepo, WorkspaceRepo};
    let host = sun_core::model::Host {
        id: HostId::generate(),
        name: name.into(),
        capabilities: serde_json::json!({
            "os": "linux", "arch": "x86_64", "cpus": 4,
            "runtimes": [{"adapter": "fake", "readiness":
                {"installed": true, "version": "1", "logged_in": logged_in, "detail": null}}],
        }),
        enrolled_by: None,
    };
    let mut tx = store.begin().await.unwrap();
    tx.insert_host(&host).await.unwrap();
    tx.link(
        sun_core::WorkspaceId::MAIN,
        sun_store::WorkspaceItem::Host(host.id),
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    host.id
}

/// The account key of `tool`'s own login on `host`, as failover cools it.
fn login(host: HostId, tool: &str) -> String {
    Account::Login(tool.into()).key(host)
}

async fn placed_on(store: &Store, task: TaskId) -> Option<HostId> {
    let (run, _) = run_of(store, task).await?;
    let events = store
        .read()
        .await
        .unwrap()
        .events_after(Some(run), 0, 16)
        .await
        .unwrap();
    let placed = events.iter().find(|e| e.kind == "run_placed")?;
    serde_json::from_value(placed.payload["host"].clone()).ok()
}

#[tokio::test(flavor = "multi_thread")]
async fn queue_places_runs_on_ready_online_hosts_within_host_caps() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).await.unwrap();
    let parsed = sun_workflow::parse(WORKFLOW).unwrap();
    let workflow = Workflow {
        id: WorkflowId::generate(),
        workspace_id: sun_core::WorkspaceId::MAIN,
        name: parsed.name.clone(),
    };
    let tasks: Vec<Task> = (0..3)
        .map(|i| Task {
            title: format!("t{i}"),
            workflow_id: Some(workflow.id),
            status: TaskStatus::Backlog,
            ..sun_core::model::Task::new(sun_core::WorkspaceId::MAIN, "")
        })
        .collect();
    let mut tx = store.begin().await.unwrap();
    tx.insert_workflow(&workflow).await.unwrap();
    tx.save_workflow_version(workflow.id, WORKFLOW, &parsed.hash)
        .await
        .unwrap();
    for task in &tasks {
        tx.insert_task(task).await.unwrap();
    }
    tx.commit().await.unwrap();

    let offline = add_host(&store, "offline", true).await;
    let logged_out = add_host(&store, "logged-out", false).await;
    let h1 = add_host(&store, "h1", true).await;
    let h2 = add_host(&store, "h2", true).await;
    let link = std::sync::Arc::new(Online(std::sync::Mutex::new(vec![logged_out, h1, h2])));
    let (engine, _engine_task) = Engine::start(
        store.clone(),
        link.clone(),
        std::sync::Arc::new(NoMods),
        EngineConfig {
            allow_bypass: false,
            host: offline,
            adapter: "fake".into(),
            delivery_timeout: Duration::from_secs(5),
            watchdog: WatchdogConfig::default(),
        },
    )
    .await
    .unwrap();
    let (queue, _queue_task) = Queue::start(
        store.clone(),
        engine.clone(),
        QueueConfig {
            slot: Slot {
                runtime: "fake".into(),
                host: offline,
            },
            caps: Caps {
                per_host: BTreeMap::from(["h1", "h2"].map(|name| {
                    let one = HostCap {
                        runs: Some(1),
                        ..HostCap::default()
                    };
                    (name.to_owned(), one)
                })),
                ..Caps::default()
            },
        },
    );

    for task in &tasks {
        queue.enqueue(task.id).await.unwrap();
    }
    let (a, b, c) = (tasks[0].id, tasks[1].id, tasks[2].id);
    eventually("a and b running", || async {
        started(&store, a).await && started(&store, b).await
    })
    .await;
    let mut placed = [placed_on(&store, a).await, placed_on(&store, b).await];
    placed.sort();
    let mut want = [Some(h1), Some(h2)];
    want.sort();
    assert_eq!(placed, want, "one run per ready, online host (cap 1)");
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(
        task_status(&store, c).await,
        TaskStatus::Todo,
        "both caps full"
    );

    // A host joins while the kernel runs: the queued run goes there.
    let h3 = add_host(&store, "h3", true).await;
    link.0.lock().unwrap().push(h3);
    queue.wake();
    eventually("c running", || async { started(&store, c).await }).await;
    assert_eq!(placed_on(&store, c).await, Some(h3));
    approve(&store, &engine, a).await;

    // Marketplace: a leased task waits for the seller's host that
    // joined with its token, and that host takes nothing else.
    let task = |title: &str| Task {
        title: title.into(),
        workflow_id: Some(workflow.id),
        status: TaskStatus::Backlog,
        ..sun_core::model::Task::new(sun_core::WorkspaceId::MAIN, "")
    };
    let (d, e) = (task("leased"), task("own"));
    let mut tx = store.begin().await.unwrap();
    tx.insert_task(&d).await.unwrap();
    tx.insert_task(&e).await.unwrap();
    tx.commit().await.unwrap();
    let vm = |network: Option<Vec<String>>| {
        sun_core::ExecEnvSpec::Vm(sun_core::VmSpec {
            template: "default".into(),
            memory_mb: None,
            cpus: None,
            network,
            keep: false,
        })
    };
    let open_vm = engine.lease_task(d.id, "lea_1", &vm(None)).await;
    assert!(open_vm.is_err(), "a leased VM needs an allowlist");
    let allowlist = vm(Some(vec!["api.anthropic.com".into()]));
    engine.lease_task(d.id, "lea_1", &allowlist).await.unwrap();
    assert!(engine.lease_task(d.id, "lea_2", &allowlist).await.is_err());
    let seller = add_host(&store, "seller", true).await;
    let vm_ready = serde_json::json!({
        "os": "linux", "arch": "x86_64", "cpus": 4,
        "runtimes": [{"adapter": "fake", "readiness":
            {"installed": true, "version": "1", "logged_in": true, "detail": null}}],
        "exec_envs": [{"kind": "vm", "ready": true, "version": "1", "detail": null}],
    });
    engine
        .set_host_capabilities(seller, &vm_ready)
        .await
        .unwrap();
    engine.bind_market_host(d.id, seller).await.unwrap();
    let lease = engine.task_lease(d.id).await.unwrap().unwrap();
    assert_eq!(
        (lease.lease_id.as_deref(), lease.host_id),
        (Some("lea_1"), Some(seller))
    );
    // Only the seller's host is online: the leased task runs there, the
    // buyer's own task keeps waiting for one of the buyer's hosts.
    *link.0.lock().unwrap() = vec![seller];
    queue.enqueue(e.id).await.unwrap();
    queue.enqueue(d.id).await.unwrap();
    eventually("the leased task running", || async {
        started(&store, d.id).await
    })
    .await;
    assert_eq!(placed_on(&store, d.id).await, Some(seller));
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(
        task_status(&store, e.id).await,
        TaskStatus::Todo,
        "the seller's host is not the buyer's"
    );
}

/// Lease before start: a queued task held for a lease stays off the
/// buyer's free hosts, even when started directly (the queue's read-then-
/// start gap), until the hold is dropped; a lease fills the hold and keeps
/// a seller host that joined it.
#[tokio::test(flavor = "multi_thread")]
async fn marketplace_hold_keeps_a_queued_task_off_free_buyer_hosts() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).await.unwrap();
    let parsed = sun_workflow::parse(WORKFLOW).unwrap();
    let workflow = Workflow {
        id: WorkflowId::generate(),
        workspace_id: sun_core::WorkspaceId::MAIN,
        name: parsed.name.clone(),
    };
    let task = |title: &str| Task {
        title: title.into(),
        workflow_id: Some(workflow.id),
        status: TaskStatus::Backlog,
        ..sun_core::model::Task::new(sun_core::WorkspaceId::MAIN, "")
    };
    let (held, filled) = (task("held"), task("filled"));
    let mut tx = store.begin().await.unwrap();
    tx.insert_workflow(&workflow).await.unwrap();
    tx.save_workflow_version(workflow.id, WORKFLOW, &parsed.hash)
        .await
        .unwrap();
    tx.insert_task(&held).await.unwrap();
    tx.insert_task(&filled).await.unwrap();
    tx.commit().await.unwrap();
    let buyer = add_host(&store, "buyer", true).await;
    let link = std::sync::Arc::new(Online(std::sync::Mutex::new(vec![])));
    let (engine, _engine_task) = Engine::start(
        store.clone(),
        link.clone(),
        std::sync::Arc::new(NoMods),
        EngineConfig {
            allow_bypass: false,
            host: buyer,
            adapter: "fake".into(),
            delivery_timeout: Duration::from_secs(5),
            watchdog: WatchdogConfig::default(),
        },
    )
    .await
    .unwrap();
    let (queue, _queue_task) = Queue::start(
        store.clone(),
        engine.clone(),
        QueueConfig {
            slot: Slot {
                runtime: "fake".into(),
                host: buyer,
            },
            caps: Caps::default(),
        },
    );

    // Queued while no host is free, then held for a lease.
    let run = queue.enqueue(held.id).await.unwrap();
    engine
        .hold_for_lease(held.id, TaskLease::default())
        .await
        .unwrap();
    assert!(
        engine
            .hold_for_lease(held.id, TaskLease::default())
            .await
            .is_err(),
        "held once"
    );
    // The buyer's host comes free: the held task still waits.
    link.0.lock().unwrap().push(buyer);
    queue.wake();
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(task_status(&store, held.id).await, TaskStatus::Todo);
    // Even a start the queue decided before the hold committed is refused.
    assert!(engine.start_run_on(run, buyer).await.is_err());
    assert_eq!(
        run_of(&store, held.id).await,
        Some((run, RunStatus::Queued))
    );
    // The lease failed: the hold is dropped and the buyer's host runs it.
    engine.release_lease_hold(held.id).await.unwrap();
    assert_eq!(engine.task_lease(held.id).await.unwrap(), None);
    queue.wake();
    eventually("the released task running", || async {
        started(&store, held.id).await
    })
    .await;
    assert_eq!(placed_on(&store, held.id).await, Some(buyer));

    // The seller's host may join before the lease is pinned: it stays bound.
    let seller = add_host(&store, "seller", true).await;
    engine
        .hold_for_lease(filled.id, TaskLease::default())
        .await
        .unwrap();
    engine.bind_market_host(filled.id, seller).await.unwrap();
    let vm = sun_core::ExecEnvSpec::Vm(sun_core::VmSpec {
        template: "default".into(),
        memory_mb: None,
        cpus: None,
        network: Some(vec!["api.anthropic.com".into()]),
        keep: false,
    });
    engine.lease_task(filled.id, "lea_2", &vm).await.unwrap();
    engine.release_lease_hold(filled.id).await.unwrap();
    let lease = engine.task_lease(filled.id).await.unwrap().unwrap();
    assert_eq!(
        (lease.lease_id.as_deref(), lease.host_id),
        (Some("lea_2"), Some(seller)),
        "a filled lease outlives a late release"
    );

    // `sun serve` start drops holds a crash left unfilled.
    let stranded = task("stranded");
    let mut tx = store.begin().await.unwrap();
    tx.insert_task(&stranded).await.unwrap();
    tx.commit().await.unwrap();
    engine
        .hold_for_lease(stranded.id, TaskLease::default())
        .await
        .unwrap();
    queue.enqueue(stranded.id).await.unwrap();
    assert_eq!(engine.clear_lease_holds().await.unwrap(), 1);
    assert_eq!(engine.task_lease(stranded.id).await.unwrap(), None);
    assert!(
        engine.task_lease(filled.id).await.unwrap().is_some(),
        "a filled lease isn't a hold"
    );
}

const MARKET_WORKFLOW: &str = r#"
name: rented
runtime:
  chain: [fake, "market:fake"]
steps:
  - id: approve
    type: approval
    who: [role:admin]
"#;

/// Failover to a marketplace runtime: once every own
/// runtime of the chain cools, a queued task is held and journalled for a
/// lease, within the ecosystem policy; leased, it runs on the seller's host.
#[tokio::test(flavor = "multi_thread")]
async fn chain_fails_over_to_market_runtime() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).await.unwrap();
    let parsed = sun_workflow::parse(MARKET_WORKFLOW).unwrap();
    let workflow = Workflow {
        id: WorkflowId::generate(),
        workspace_id: sun_core::WorkspaceId::MAIN,
        name: parsed.name.clone(),
    };
    let task = |title: &str| Task {
        title: title.into(),
        workflow_id: Some(workflow.id),
        status: TaskStatus::Backlog,
        ..sun_core::model::Task::new(sun_core::WorkspaceId::MAIN, "")
    };
    let (first, second) = (task("first"), task("second"));
    let mut tx = store.begin().await.unwrap();
    tx.insert_workflow(&workflow).await.unwrap();
    tx.save_workflow_version(workflow.id, MARKET_WORKFLOW, &parsed.hash)
        .await
        .unwrap();
    tx.insert_task(&first).await.unwrap();
    tx.insert_task(&second).await.unwrap();
    tx.commit().await.unwrap();
    let buyer = add_host(&store, "buyer", true).await;
    let link = std::sync::Arc::new(Online(std::sync::Mutex::new(vec![buyer])));
    let (engine, _engine_task) = Engine::start(
        store.clone(),
        link.clone(),
        std::sync::Arc::new(NoMods),
        EngineConfig {
            allow_bypass: false,
            host: buyer,
            adapter: "fake".into(),
            delivery_timeout: Duration::from_secs(5),
            watchdog: WatchdogConfig::default(),
        },
    )
    .await
    .unwrap();
    let (queue, _queue_task) = Queue::start(
        store.clone(),
        engine.clone(),
        QueueConfig {
            slot: Slot {
                runtime: "fake".into(),
                host: buyer,
            },
            caps: Caps::default(),
        },
    );
    // The first run takes the buyer's runtime, which then hits its limit.
    let run = queue.enqueue(first.id).await.unwrap();
    eventually("first running", || async {
        started(&store, first.id).await
    })
    .await;
    let until = chrono::Utc::now() + chrono::Duration::minutes(10);
    let account = login(buyer, "fake");
    let cooling = serde_json::json!({ "runtime": "fake", "until": until });
    let mut tx = store.begin().await.unwrap();
    tx.set_cooldown(&account, Some(until)).await.unwrap();
    tx.append_event(NewEvent::new(run, None, "runtime_cooling", cooling))
        .await
        .unwrap();
    tx.commit().await.unwrap();

    // No failover price cap yet: the second task waits for the cooldown.
    let second_run = queue.enqueue(second.id).await.unwrap();
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(engine.task_lease(second.id).await.unwrap(), None);
    // A policy that keeps this workflow off the marketplace: still waiting.
    let blocked = MarketPolicy {
        allow: sun_engine::MarketAllow::Workflows,
        workflows: vec!["other".into()],
        failover_max_price_points: Some(50),
    };
    engine.set_market_policy(&blocked).await.unwrap();
    queue.wake();
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(engine.task_lease(second.id).await.unwrap(), None);

    // Allowed with a cap: the chain fails over to `market:fake`.
    let allowed = MarketPolicy {
        workflows: vec!["rented".into()],
        ..blocked
    };
    engine.set_market_policy(&allowed).await.unwrap();
    queue.wake();
    eventually("the task held for a market lease", || async {
        engine.task_lease(second.id).await.unwrap().is_some()
    })
    .await;
    let held = engine.task_lease(second.id).await.unwrap().unwrap();
    assert_eq!(
        (held.lease_id, held.runtime.as_deref(), held.secrets.len()),
        (None, Some("fake"), 0),
        "held for the market runtime, no secret allowed"
    );
    let events = store
        .read()
        .await
        .unwrap()
        .events_after(Some(second_run), 0, 50)
        .await
        .unwrap();
    let failover = events
        .iter()
        .find(|e| e.kind == sun_engine::MARKET_FAILOVER)
        .expect("market_failover journalled");
    assert_eq!(failover.payload["runtime"], "fake");
    assert_eq!(failover.payload["max_price_points"], 50);

    // The API leases it; the seller's host joins and runs it, cooldown or not.
    let seller = add_host(&store, "seller", true).await;
    let vm_ready = serde_json::json!({
        "os": "linux", "arch": "x86_64", "cpus": 4,
        "runtimes": [{"adapter": "fake", "readiness":
            {"installed": true, "version": "1", "logged_in": true, "detail": null}}],
        "exec_envs": [{"kind": "vm", "ready": true, "version": "1", "detail": null}],
    });
    engine
        .set_host_capabilities(seller, &vm_ready)
        .await
        .unwrap();
    engine.bind_market_host(second.id, seller).await.unwrap();
    let vm = sun_core::ExecEnvSpec::Vm(sun_core::VmSpec {
        template: "default".into(),
        memory_mb: None,
        cpus: None,
        network: Some(vec!["api.anthropic.com".into()]),
        keep: false,
    });
    engine.lease_task(second.id, "lea_9", &vm).await.unwrap();
    link.0.lock().unwrap().push(seller);
    queue.wake();
    eventually("the failed-over task running", || async {
        started(&store, second.id).await
    })
    .await;
    assert_eq!(placed_on(&store, second.id).await, Some(seller));
    let leased = engine.task_lease(second.id).await.unwrap().unwrap();
    assert_eq!(
        leased.runtime.as_deref(),
        Some("fake"),
        "the hold's runtime stays"
    );
}

/// While the ecosystem's monthly spend cap is reached, a run with an
/// agent step stays queued (one without spends nothing and runs); raising
/// the cap lets it start.
#[tokio::test(flavor = "multi_thread")]
async fn ecosystem_spend_cap_holds_new_steps() {
    const AGENT: &str = "
name: spender
steps:
  - id: build
    type: agent
    prompt: build it
";
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).await.unwrap();
    let mut tx = store.begin().await.unwrap();
    let mut tasks = Vec::new();
    for yaml in [AGENT, WORKFLOW] {
        let parsed = sun_workflow::parse(yaml).unwrap();
        let workflow = Workflow {
            id: WorkflowId::generate(),
            workspace_id: sun_core::WorkspaceId::MAIN,
            name: parsed.name.clone(),
        };
        tx.insert_workflow(&workflow).await.unwrap();
        tx.save_workflow_version(workflow.id, yaml, &parsed.hash)
            .await
            .unwrap();
        let task = Task {
            title: parsed.name.clone(),
            workflow_id: Some(workflow.id),
            status: TaskStatus::Backlog,
            ..sun_core::model::Task::new(sun_core::WorkspaceId::MAIN, "")
        };
        tx.insert_task(&task).await.unwrap();
        tasks.push(task.id);
    }
    tx.commit().await.unwrap();
    let (spender, gated) = (tasks[0], tasks[1]);

    let host = HostId::generate();
    let (engine, _engine_task) = Engine::start(
        store.clone(),
        std::sync::Arc::new(NoHost),
        std::sync::Arc::new(NoMods),
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
    let (queue, _queue_task) = Queue::start(
        store.clone(),
        engine.clone(),
        QueueConfig {
            slot: Slot {
                runtime: "fake".into(),
                host,
            },
            caps: Caps::default(),
        },
    );

    engine.set_spend_cap(Some(0.0)).await.unwrap();
    let held = queue.enqueue(spender).await.unwrap();
    queue.enqueue(gated).await.unwrap();
    eventually("the approval-only run started", || async {
        started(&store, gated).await
    })
    .await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(
        run_of(&store, spender).await,
        Some((held, RunStatus::Queued))
    );
    assert_eq!(engine.spend().await.unwrap().monthly_usd, Some(0.0));

    engine.set_spend_cap(Some(10.0)).await.unwrap();
    queue.wake();
    eventually("the held run started", || async {
        run_of(&store, spender)
            .await
            .is_some_and(|(_, s)| s != RunStatus::Queued)
    })
    .await;
}

/// The clock of [`spend_cap_period_rollover_wakes_the_queue`]: wall time
/// shifted by this many milliseconds.
static CLOCK_SHIFT_MS: AtomicI64 = AtomicI64::new(0);

fn shifted_now() -> sun_core::Timestamp {
    chrono::Utc::now() + chrono::Duration::milliseconds(CLOCK_SHIFT_MS.load(Ordering::SeqCst))
}

/// A run held by the monthly cap starts when the month rolls over,
/// with no other wake-up. The queue's clock starts 3 s before this month's
/// start, and last month's spend reached the cap; the executor's real clock
/// is in the new month already, so the agent step really starts.
#[tokio::test(flavor = "multi_thread")]
async fn spend_cap_period_rollover_wakes_the_queue() {
    use chrono::{Datelike, TimeZone};
    const AGENT: &str = "
name: spender
steps:
  - id: build
    type: agent
    prompt: build it
";
    let real = chrono::Utc::now();
    let boundary = chrono::Utc
        .with_ymd_and_hms(real.year(), real.month(), 1, 0, 0, 0)
        .unwrap();
    let shift = (boundary - chrono::Duration::seconds(3)) - real;
    CLOCK_SHIFT_MS.store(shift.num_milliseconds(), Ordering::SeqCst);

    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).await.unwrap();
    let mut tx = store.begin().await.unwrap();
    let mut tasks = Vec::new();
    for yaml in [AGENT, WORKFLOW] {
        let parsed = sun_workflow::parse(yaml).unwrap();
        let workflow = Workflow {
            id: WorkflowId::generate(),
            workspace_id: sun_core::WorkspaceId::MAIN,
            name: parsed.name.clone(),
        };
        tx.insert_workflow(&workflow).await.unwrap();
        tx.save_workflow_version(workflow.id, yaml, &parsed.hash)
            .await
            .unwrap();
        let task = Task {
            title: parsed.name.clone(),
            workflow_id: Some(workflow.id),
            status: TaskStatus::Backlog,
            ..sun_core::model::Task::new(sun_core::WorkspaceId::MAIN, "")
        };
        tx.insert_task(&task).await.unwrap();
        tasks.push(task.id);
    }
    tx.commit().await.unwrap();
    let (spender, gated) = (tasks[0], tasks[1]);

    let host = HostId::generate();
    let (engine, _engine_task) = Engine::start(
        store.clone(),
        std::sync::Arc::new(NoHost),
        std::sync::Arc::new(NoMods),
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
    let (queue, _queue_task) = Queue::start_with_clock(
        store.clone(),
        engine.clone(),
        QueueConfig {
            slot: Slot {
                runtime: "fake".into(),
                host,
            },
            caps: Caps::default(),
        },
        shifted_now,
    );

    // Last month's spend ($2 against a $1 cap), on the approval-only run.
    engine.set_spend_cap(Some(1.0)).await.unwrap();
    queue.enqueue(gated).await.unwrap();
    eventually("the approval-only run started", || async {
        started(&store, gated).await
    })
    .await;
    let (gated_run, _) = run_of(&store, gated).await.unwrap();
    let session = SessionId::generate();
    let last_month = boundary - chrono::Duration::hours(1);
    let mut tx = store.begin().await.unwrap();
    let mut assigned = NewEvent::new(
        gated_run,
        None,
        "session_assigned",
        serde_json::json!({ "session_id": session, "adapter": "fake" }),
    );
    assigned.at = last_month;
    tx.append_event(assigned).await.unwrap();
    let mut usage = NewEvent::new(
        gated_run,
        None,
        "usage",
        serde_json::json!({ "input": 1, "output": 1, "cost_usd": 2.0 }),
    );
    usage.at = last_month;
    tx.append_host_event(session, 1, usage).await.unwrap();
    tx.commit().await.unwrap();

    let held = queue.enqueue(spender).await.unwrap();
    // Held for the rest of the queue's month; nothing else wakes it.
    assert!(
        shifted_now() < boundary - chrono::Duration::seconds(1),
        "setup took too long to see the hold"
    );
    while shifted_now() < boundary - chrono::Duration::milliseconds(200) {
        assert_eq!(
            run_of(&store, spender).await,
            Some((held, RunStatus::Queued)),
            "held by the cap until the month ends"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    eventually("the agent step started in the new month", || async {
        let events = store
            .read()
            .await
            .unwrap()
            .events_after(Some(held), 0, 100)
            .await
            .unwrap();
        events
            .iter()
            .any(|e| e.kind == "step_status" && e.payload["to"] == "dispatched")
    })
    .await;
    assert!(shifted_now() >= boundary, "started only after the rollover");
}

/// A cooldown whose `runtime_cooling` event the queue missed (its receiver
/// lagged) still holds runs: the queue re-reads `runtime_state`.
// current_thread: `commit` broadcasts in one synchronous loop, so more events
// than the channel holds always lag the queue's receiver.
#[tokio::test(flavor = "current_thread")]
async fn lagged_queue_reloads_cooldowns_from_the_store() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).await.unwrap();
    let parsed = sun_workflow::parse(WORKFLOW).unwrap();
    let workflow = Workflow {
        id: WorkflowId::generate(),
        workspace_id: sun_core::WorkspaceId::MAIN,
        name: parsed.name.clone(),
    };
    let task = |title: &str| Task {
        title: title.into(),
        workflow_id: Some(workflow.id),
        status: TaskStatus::Backlog,
        ..sun_core::model::Task::new(sun_core::WorkspaceId::MAIN, "")
    };
    let (first, second) = (task("first"), task("second"));
    let mut tx = store.begin().await.unwrap();
    tx.insert_workflow(&workflow).await.unwrap();
    tx.save_workflow_version(workflow.id, WORKFLOW, &parsed.hash)
        .await
        .unwrap();
    tx.insert_task(&first).await.unwrap();
    tx.insert_task(&second).await.unwrap();
    tx.commit().await.unwrap();
    let host = HostId::generate();
    let (engine, _engine_task) = Engine::start(
        store.clone(),
        std::sync::Arc::new(NoHost),
        std::sync::Arc::new(NoMods),
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
    let (queue, _queue_task) = Queue::start(
        store.clone(),
        engine.clone(),
        QueueConfig {
            slot: Slot {
                runtime: "fake".into(),
                host,
            },
            caps: Caps::default(),
        },
    );
    let run = queue.enqueue(first.id).await.unwrap();
    eventually("first running", || async {
        started(&store, first.id).await
    })
    .await;

    // The cooldown lands only in `account_cooldown`, under more events than
    // the queue's receiver holds.
    let mut tx = store.begin().await.unwrap();
    let until = chrono::Utc::now() + chrono::Duration::minutes(10);
    tx.set_cooldown(&login(host, "fake"), Some(until))
        .await
        .unwrap();
    for _ in 0..1100 {
        tx.append_event(NewEvent::new(run, None, "filler", Value::Null))
            .await
            .unwrap();
    }
    tx.commit().await.unwrap();

    let second_run = queue.enqueue(second.id).await.unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(
        run_of(&store, second.id).await,
        Some((second_run, RunStatus::Queued)),
        "waits out the cooldown it never saw an event for"
    );
}

/// The clock of [`cooling_runtime_never_starts_a_run_under_random_interleavings`]:
/// wall time shifted by this many milliseconds, so a round ends its cooldown
/// by moving the clock past it.
static COOL_SHIFT_MS: AtomicI64 = AtomicI64::new(0);

fn cool_now() -> sun_core::Timestamp {
    chrono::Utc::now() + chrono::Duration::milliseconds(COOL_SHIFT_MS.load(Ordering::SeqCst))
}

/// B4f3: a run enqueued after a cooldown committed never starts while it
/// lasts, however the commit, the enqueue and the queue's passes interleave:
/// enqueued at once, after a yield or a short sleep, beside other enqueues,
/// with the cooldown as an event only or also in `runtime_state` (then
/// sometimes under more events than the queue's receiver holds). Each round then moves the
/// clock past the cooldown and checks the run starts. Seeded from the clock;
/// a failure names its seed.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cooling_runtime_never_starts_a_run_under_random_interleavings() {
    let seed = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos() as u64
        | 1;
    // xorshift64: enough to vary the interleavings without a dependency.
    let mut state = seed;
    let mut random = move |n: u64| {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state % n
    };
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).await.unwrap();
    let parsed = sun_workflow::parse(WORKFLOW).unwrap();
    let workflow = Workflow {
        id: WorkflowId::generate(),
        workspace_id: sun_core::WorkspaceId::MAIN,
        name: parsed.name.clone(),
    };
    let mut tx = store.begin().await.unwrap();
    tx.insert_workflow(&workflow).await.unwrap();
    tx.save_workflow_version(workflow.id, WORKFLOW, &parsed.hash)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    let new_task = |title: String| {
        let store = store.clone();
        async move {
            let task = Task {
                workflow_id: Some(workflow.id),
                status: TaskStatus::Backlog,
                ..Task::new(sun_core::WorkspaceId::MAIN, title)
            };
            let mut tx = store.begin().await.unwrap();
            tx.insert_task(&task).await.unwrap();
            tx.commit().await.unwrap();
            task.id
        }
    };
    let host = HostId::generate();
    let (engine, _engine_task) = Engine::start(
        store.clone(),
        std::sync::Arc::new(NoHost),
        std::sync::Arc::new(NoMods),
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
    let (queue, _queue_task) = Queue::start_with_clock(
        store.clone(),
        engine.clone(),
        QueueConfig {
            slot: Slot {
                runtime: "fake".into(),
                host,
            },
            caps: Caps::default(),
        },
        cool_now,
    );
    // The run the cooldown events are journaled on, as failover does.
    let anchor = new_task("anchor".into()).await;
    let anchor_run = queue.enqueue(anchor).await.unwrap();
    eventually("anchor running", || async { started(&store, anchor).await }).await;

    for round in 0..25 {
        let what = format!("seed {seed}, round {round}");
        let held = new_task(format!("held {round}")).await;
        let racer = new_task(format!("racer {round}")).await;
        let releaser = new_task(format!("releaser {round}")).await;
        let until = cool_now() + chrono::Duration::minutes(10);
        // The cooldown is in `account_cooldown` with its event, as failover
        // writes it; fillers may make the receiver lag past the event.
        let fillers = random(3) * 700;
        // Another enqueue racing the cooldown's commit (it may start or not).
        let racing = {
            let queue = queue.clone();
            let pause = random(3);
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(pause)).await;
                queue.enqueue(racer).await.unwrap();
            })
        };
        let mut tx = store.begin().await.unwrap();
        let account = login(host, "fake");
        tx.set_cooldown(&account, Some(until)).await.unwrap();
        let cooling = serde_json::json!({ "runtime": "fake", "until": until });
        tx.append_event(NewEvent::new(anchor_run, None, "runtime_cooling", cooling))
            .await
            .unwrap();
        for _ in 0..fillers {
            tx.append_event(NewEvent::new(anchor_run, None, "filler", Value::Null))
                .await
                .unwrap();
        }
        tx.commit().await.unwrap();
        match random(3) {
            0 => {}
            1 => tokio::task::yield_now().await,
            _ => tokio::time::sleep(Duration::from_millis(random(4))).await,
        }
        let held_run = queue.enqueue(held).await.unwrap();
        let watch = Instant::now() + Duration::from_millis(20 + random(40));
        while Instant::now() < watch {
            assert_eq!(
                run_of(&store, held).await,
                Some((held_run, RunStatus::Queued)),
                "{what}: started on a cooling runtime (fillers: {fillers})"
            );
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        racing.await.unwrap();

        // Past the cooldown, the next wake-up starts it.
        COOL_SHIFT_MS.fetch_add(11 * 60 * 1000, Ordering::SeqCst);
        queue.enqueue(releaser).await.unwrap();
        eventually(&format!("{what}: held run starts after"), || async {
            started(&store, held).await
        })
        .await;
    }
}

/// An engine and queue on `link`'s hosts, runtime `fake`, whose operator
/// allows `bypass_permissions` (so the bare-host rule is what refuses it).
async fn kernel_on(
    store: &Store,
    link: std::sync::Arc<Online>,
    fallback: HostId,
    caps: Caps,
) -> (Engine, Queue) {
    kernel_allowing(store, link, fallback, caps, true).await
}

/// [`kernel_on`] with sun.yaml `runtimes.allow_bypass` set to `allow_bypass`.
async fn kernel_allowing(
    store: &Store,
    link: std::sync::Arc<Online>,
    fallback: HostId,
    caps: Caps,
    allow_bypass: bool,
) -> (Engine, Queue) {
    let (engine, _engine_task) = Engine::start(
        store.clone(),
        link,
        std::sync::Arc::new(NoMods),
        EngineConfig {
            allow_bypass,
            host: fallback,
            adapter: "fake".into(),
            delivery_timeout: Duration::from_secs(5),
            watchdog: WatchdogConfig::default(),
        },
    )
    .await
    .unwrap();
    let slot = Slot {
        runtime: "fake".into(),
        host: fallback,
    };
    let (queue, _queue_task) =
        Queue::start(store.clone(), engine.clone(), QueueConfig { slot, caps });
    (engine, queue)
}

/// `count` backlog tasks on a new workflow of `source`.
async fn tasks_of(store: &Store, source: &str, count: usize) -> Vec<Task> {
    let parsed = sun_workflow::parse(source).unwrap();
    let workflow = Workflow {
        id: WorkflowId::generate(),
        workspace_id: sun_core::WorkspaceId::MAIN,
        name: parsed.name.clone(),
    };
    let tasks: Vec<Task> = (0..count)
        .map(|i| Task {
            title: format!("t{i}"),
            workflow_id: Some(workflow.id),
            status: TaskStatus::Backlog,
            ..sun_core::model::Task::new(sun_core::WorkspaceId::MAIN, "")
        })
        .collect();
    let mut tx = store.begin().await.unwrap();
    tx.insert_workflow(&workflow).await.unwrap();
    tx.save_workflow_version(workflow.id, source, &parsed.hash)
        .await
        .unwrap();
    for task in &tasks {
        tx.insert_task(task).await.unwrap();
    }
    tx.commit().await.unwrap();
    tasks
}

/// Item 9: a step's own `permission_mode:` (an agent's, on its system
/// workflow) decides at placement. Bypass on the step on a bare-only
/// workspace takes the FR7 refusal path, never a silent downgrade; `plan`
/// on the step places.
#[tokio::test(flavor = "multi_thread")]
async fn queue_a_steps_permission_mode_wins_at_placement() {
    const BYPASS: &str = "name: bypass\nruntime: {chain: [plain]}\nsteps:\n  \
                          - {id: implement, type: agent, prompt: do it, permission_mode: bypass_permissions}\n";
    const PLAN: &str = "name: plan\nsteps:\n  \
                        - {id: implement, type: agent, prompt: do it, permission_mode: plan}\n";
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).await.unwrap();
    let bypass = tasks_of(&store, BYPASS, 1).await.remove(0);
    let plan = tasks_of(&store, PLAN, 1).await.remove(0);
    let host = add_host(&store, "bare", true).await;
    let link = std::sync::Arc::new(Online(std::sync::Mutex::new(vec![host])));
    let (engine, queue) = kernel_on(&store, link, host, Caps::default()).await;

    queue.enqueue(bypass.id).await.unwrap();
    queue.enqueue(plan.id).await.unwrap();
    eventually("the plan run placed", || async {
        placed_on(&store, plan.id).await.is_some()
    })
    .await;
    let failed = || async {
        let mut read = store.read().await.unwrap();
        let run = read.runs_of_task(bypass.id).await.unwrap().pop();
        run.filter(|r| r.status == RunStatus::Failed)
    };
    eventually("the bypass run refused", || async {
        failed().await.is_some()
    })
    .await;
    let run = failed().await.unwrap();
    let events = store
        .read()
        .await
        .unwrap()
        .events_after(Some(run.id), 0, 32)
        .await
        .unwrap();
    let refused = events
        .iter()
        .find(|e| e.kind == "placement_refused_bypass_bare")
        .expect("journalled");
    assert_eq!(refused.payload["runtime"], "plain");
    assert!(
        !events.iter().any(|e| e.kind == "run_placed"),
        "never placed"
    );
    // Nobody was capped, so the thread says only why (BLO-175).
    let why = refused.payload["reason"].as_str().unwrap();
    let line = format!("This run failed before it started: {why}.");
    let activity = engine.activity(bypass.id, 0).await.unwrap();
    assert!(
        activity.iter().any(|a| a.body["text"] == line.as_str()),
        "thread doesn't say why: {activity:#?}"
    );
}

/// BLO-164 gate order: the triggering member's cap (3) comes before the
/// container/VM rule (4). A member the policy doesn't allow is capped to
/// `accept_edits`, so their bypass run on a bare host is placed, not
/// refused; the owner's same run is still refused there.
#[tokio::test(flavor = "multi_thread")]
async fn queue_places_a_capped_members_bypass_run_on_a_bare_host() {
    use sun_store::MemberRepo;
    const BYPASS: &str = "name: bypass\nruntime: {chain: [plain]}\nsteps:\n  \
                          - {id: implement, type: agent, prompt: do it, permission_mode: bypass_permissions}\n";
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).await.unwrap();
    let mut tasks = tasks_of(&store, BYPASS, 2).await;
    let (member_task, owner_task) = (tasks.remove(0), tasks.remove(0));
    let mut tx = store.begin().await.unwrap();
    let ana = tx
        .upsert_member("local", "ana@sun.test", "ana", Some("ana@sun.test"), false)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    let host = add_host(&store, "bare", true).await;
    let link = std::sync::Arc::new(Online(std::sync::Mutex::new(vec![host])));
    let (_engine, queue) = kernel_on(&store, link, host, Caps::default()).await;

    let by = sun_engine::Actor {
        member_id: Some(ana.id),
        name: "user:ana".into(),
    };
    queue.enqueue_by(member_task.id, Some(&by)).await.unwrap();
    queue.enqueue(owner_task.id).await.unwrap();
    eventually("the member's capped run placed", || async {
        placed_on(&store, member_task.id).await.is_some()
    })
    .await;
    let failed = || async {
        let mut read = store.read().await.unwrap();
        let run = read.runs_of_task(owner_task.id).await.unwrap().pop();
        run.filter(|r| r.status == RunStatus::Failed)
    };
    eventually("the owner's bypass run refused on bare", || async {
        failed().await.is_some()
    })
    .await;
}

/// FR7: a run whose step is in `bypass_permissions` goes only to a host
/// with its container ready; a bare run of it is refused, journalled, and
/// fails without being placed, even with a container host online.
#[tokio::test(flavor = "multi_thread")]
async fn queue_places_bypass_runs_only_in_a_container_or_vm() {
    const AGENT: &str = "name: agentic\nsteps:\n  \
                         - {id: implement, type: agent, prompt: do it, permission_mode: bypass_permissions}\n";
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).await.unwrap();
    let tasks = tasks_of(&store, AGENT, 2).await;
    let (boxed, bare) = (&tasks[0], &tasks[1]);
    let mut tx = store.begin().await.unwrap();
    let container = serde_json::json!({"kind": "container", "image": "toolchain:1"});
    tx.put_setting(&format!("task_exec_env.{}", boxed.id), &container)
        .await
        .unwrap();
    tx.commit().await.unwrap();

    let bare_host = add_host(&store, "bare", true).await;
    let docker_host = add_host(&store, "docker", true).await;
    let link = std::sync::Arc::new(Online(std::sync::Mutex::new(vec![bare_host, docker_host])));
    let (engine, queue) = kernel_on(&store, link, bare_host, Caps::default()).await;
    let docker_ready = serde_json::json!({
        "os": "linux", "arch": "x86_64", "cpus": 4,
        "runtimes": [{"adapter": "fake", "readiness":
            {"installed": true, "version": "1", "logged_in": true, "detail": null}}],
        "exec_envs": [{"kind": "container", "ready": true, "version": "27", "detail": null}],
    });
    engine
        .set_host_capabilities(docker_host, &docker_ready)
        .await
        .unwrap();

    queue.enqueue(boxed.id).await.unwrap();
    queue.enqueue(bare.id).await.unwrap();
    eventually("the container run placed", || async {
        placed_on(&store, boxed.id).await.is_some()
    })
    .await;
    assert_eq!(placed_on(&store, boxed.id).await, Some(docker_host));

    let failed = || async {
        let mut read = store.read().await.unwrap();
        let run = read.runs_of_task(bare.id).await.unwrap().pop();
        run.filter(|r| r.status == RunStatus::Failed)
    };
    eventually("the bare run refused", || async {
        failed().await.is_some()
    })
    .await;
    let run = failed().await.unwrap();
    let reason = run.end_reason.unwrap_or_default();
    assert!(
        reason.contains("bypass_permissions needs a container or vm"),
        "{reason}"
    );
    let events = store
        .read()
        .await
        .unwrap()
        .events_after(Some(run.id), 0, 32)
        .await
        .unwrap();
    let refused = events
        .iter()
        .find(|e| e.kind == "placement_refused_bypass_bare")
        .expect("journalled");
    assert_eq!(refused.payload["exec_env"], "bare");
    assert_eq!(refused.payload["runtime"], "fake");
    assert!(
        !events.iter().any(|e| e.kind == "run_placed"),
        "never placed"
    );
    assert!(
        !events.iter().any(|e| e.kind == "session_assigned"),
        "no session"
    );
}

/// BLO-139 third path: an agent saved in `bypass_permissions` while the
/// operator allowed it (its system workflow's step carries the mode) stops
/// running bypass once sun.yaml `runtimes.allow_bypass` is off, even in a
/// container: placement refuses it with its own reason and it never gets a
/// session. With allow on, the same run is placed on the container host.
#[tokio::test(flavor = "multi_thread")]
async fn queue_refuses_bypass_while_the_operator_turned_it_off() {
    const AGENT: &str = "name: agent\nexec_env: {kind: container, image: 'node:22'}\nsteps:\n  \
                         - {id: implement, type: agent, prompt: do it, permission_mode: bypass_permissions}\n";
    for allow_bypass in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).await.unwrap();
        let task = tasks_of(&store, AGENT, 1).await.remove(0);
        let docker_host = add_host(&store, "docker", true).await;
        let link = std::sync::Arc::new(Online(std::sync::Mutex::new(vec![docker_host])));
        let (engine, queue) =
            kernel_allowing(&store, link, docker_host, Caps::default(), allow_bypass).await;
        let docker_ready = serde_json::json!({
            "os": "linux", "arch": "x86_64", "cpus": 4,
            "runtimes": [{"adapter": "fake", "readiness":
                {"installed": true, "version": "1", "logged_in": true, "detail": null}}],
            "exec_envs": [{"kind": "container", "ready": true, "version": "27", "detail": null}],
        });
        engine
            .set_host_capabilities(docker_host, &docker_ready)
            .await
            .unwrap();

        queue.enqueue(task.id).await.unwrap();
        if allow_bypass {
            eventually("the bypass run placed", || async {
                placed_on(&store, task.id).await.is_some()
            })
            .await;
            assert_eq!(placed_on(&store, task.id).await, Some(docker_host));
            continue;
        }
        let failed = || async {
            let mut read = store.read().await.unwrap();
            let run = read.runs_of_task(task.id).await.unwrap().pop();
            run.filter(|r| r.status == RunStatus::Failed)
        };
        eventually("the bypass run refused", || async {
            failed().await.is_some()
        })
        .await;
        let run = failed().await.unwrap();
        let reason = run.end_reason.unwrap_or_default();
        assert!(reason.contains("runtimes.allow_bypass: false"), "{reason}");
        let events = store
            .read()
            .await
            .unwrap()
            .events_after(Some(run.id), 0, 32)
            .await
            .unwrap();
        let refused = events
            .iter()
            .find(|e| e.kind == "placement_refused_bypass_disabled")
            .expect("journalled");
        assert_eq!(refused.payload["exec_env"], "container");
        assert_eq!(refused.payload["runtime"], "fake");
        for never in [
            "placement_refused_bypass_bare",
            "run_placed",
            "session_assigned",
        ] {
            assert!(!events.iter().any(|e| e.kind == never), "{never}");
        }
    }
}

/// `runtimes.model: v2` caps each host at its reported `max_sessions` of a
/// runtime (else `caps.max_sessions`), so the next run goes to the next
/// host; flipping back to `v1` (the kill switch) drops the cap.
#[tokio::test(flavor = "multi_thread")]
async fn queue_v2_caps_sessions_per_host_and_v1_is_the_kill_switch() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).await.unwrap();
    let tasks = tasks_of(&store, WORKFLOW, 3).await;
    let set_model = |model: &'static str| {
        let store = store.clone();
        async move {
            let mut tx = store.begin().await.unwrap();
            let model = serde_json::json!(model);
            tx.put_setting(sun_engine::RUNTIMES_MODEL, &model)
                .await
                .unwrap();
            tx.commit().await.unwrap();
        }
    };
    set_model("v2").await;
    // h1 reports its own cap of 1; h2 reports none and gets sun.yaml's 1.
    let h1 = add_host(&store, "h1", true).await;
    let h2 = add_host(&store, "h2", true).await;
    let link = std::sync::Arc::new(Online(std::sync::Mutex::new(vec![h1, h2])));
    let caps = Caps {
        max_sessions: Some(1),
        ..Caps::default()
    };
    let (engine, queue) = kernel_on(&store, link, h1, caps).await;
    let capped = serde_json::json!({
        "os": "linux", "arch": "x86_64", "cpus": 4,
        "runtimes": [{"adapter": "fake", "max_sessions": 1, "readiness":
            {"installed": true, "version": "1", "logged_in": true, "detail": null}}],
    });
    engine.set_host_capabilities(h1, &capped).await.unwrap();

    for task in &tasks {
        queue.enqueue(task.id).await.unwrap();
    }
    let (a, b, c) = (tasks[0].id, tasks[1].id, tasks[2].id);
    eventually("a and b running", || async {
        started(&store, a).await && started(&store, b).await
    })
    .await;
    let mut placed = [placed_on(&store, a).await, placed_on(&store, b).await];
    placed.sort();
    let mut want = [Some(h1), Some(h2)];
    want.sort();
    assert_eq!(placed, want, "one session of `fake` per host");
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(task_status(&store, c).await, TaskStatus::Todo, "both full");

    set_model("v1").await;
    queue.wake();
    eventually("c running under v1", || async { started(&store, c).await }).await;
}

/// Item 5: a limit cools the account that hit it. Two hosts logged into
/// their own accounts of the same CLI: the first host's login cooling keeps
/// runs off it only, so the second still takes them. A key both would
/// spend cools on both: the next run waits.
#[tokio::test(flavor = "multi_thread")]
async fn queue_cools_the_account_not_every_host_with_the_tool() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).await.unwrap();
    let tasks = tasks_of(&store, WORKFLOW, 3).await;
    let h1 = add_host(&store, "h1", true).await;
    let h2 = add_host(&store, "h2", true).await;
    let until = chrono::Utc::now() + chrono::Duration::minutes(10);
    let mut tx = store.begin().await.unwrap();
    tx.set_cooldown(&login(h1, "fake"), Some(until))
        .await
        .unwrap();
    tx.commit().await.unwrap();
    let link = std::sync::Arc::new(Online(std::sync::Mutex::new(vec![h1, h2])));
    let (_engine, queue) = kernel_on(&store, link, h1, Caps::default()).await;

    let (a, b, c) = (tasks[0].id, tasks[1].id, tasks[2].id);
    let run_a = queue.enqueue(a).await.unwrap();
    queue.enqueue(b).await.unwrap();
    eventually("a and b running", || async {
        started(&store, a).await && started(&store, b).await
    })
    .await;
    assert_eq!(placed_on(&store, a).await, Some(h2), "h1's login cools");
    assert_eq!(placed_on(&store, b).await, Some(h2), "h2's doesn't");

    // `fake` now runs on a key: its cooldown holds on every host.
    let key = sun_core::model::Runtime {
        id: sun_core::RuntimeId::generate(),
        workspace_id: sun_core::WorkspaceId::MAIN,
        name: "fake".into(),
        adapter: "fake".into(),
        source: RuntimeSource::Credits,
        byo_key: None,
        created_by: None,
    };
    let account = Account::Credential(key.id).key(h1);
    let cooling = serde_json::json!({ "runtime": "fake", "until": until });
    let mut tx = store.begin().await.unwrap();
    tx.set_cooldown(&account, Some(until)).await.unwrap();
    assert!(tx.insert_runtime(&key).await.unwrap());
    tx.set_cooldown(&account, Some(until)).await.unwrap();
    tx.append_event(NewEvent::new(run_a, None, "runtime_cooling", cooling))
        .await
        .unwrap();
    tx.commit().await.unwrap();
    let run_c = queue.enqueue(c).await.unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(
        run_of(&store, c).await,
        Some((run_c, RunStatus::Queued)),
        "the key cools on h1 and h2"
    );
}

/// BLO-136 case 2a under `runtimes.model: v2`, with only bare hosts
/// online: a run whose step is in `bypass_permissions` fails at placement:
/// journalled as `placement_refused_bypass_bare` with exactly `{runtime,
/// exec_env, reason}` and no account key, never placed, no session. (Case
/// 2d, a per-tool bypass beside a plain entry, went with `runtime_policy`.)
#[tokio::test(flavor = "multi_thread")]
async fn queue_v2_bare_hosts_refuse_a_bypass_step() {
    const SOLO: &str = "name: solo\nsteps:\n  \
                        - {id: implement, type: agent, prompt: do it, permission_mode: bypass_permissions}\n";
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).await.unwrap();
    let solo = tasks_of(&store, SOLO, 1).await.remove(0);
    let mut tx = store.begin().await.unwrap();
    tx.put_setting(sun_engine::RUNTIMES_MODEL, &serde_json::json!("v2"))
        .await
        .unwrap();
    tx.commit().await.unwrap();

    let h1 = add_host(&store, "bare-1", true).await;
    let h2 = add_host(&store, "bare-2", true).await;
    let link = std::sync::Arc::new(Online(std::sync::Mutex::new(vec![h1, h2])));
    let (engine, queue) = kernel_on(&store, link, h1, Caps::default()).await;
    let ready = |adapter: &str| {
        serde_json::json!({"adapter": adapter, "readiness":
            {"installed": true, "version": "1", "logged_in": true, "detail": null}})
    };
    let bare_only = serde_json::json!({
        "os": "linux", "arch": "x86_64", "cpus": 4,
        "runtimes": [ready("fake"), ready("plain")],
        "exec_envs": [{"kind": "bare", "ready": true, "version": null, "detail": null}],
    });
    for host in [h1, h2] {
        engine
            .set_host_capabilities(host, &bare_only)
            .await
            .unwrap();
    }
    let rows = sun_store::HostRepo::host_runtimes(&mut store.read().await.unwrap(), None)
        .await
        .unwrap();
    assert_eq!(rows.len(), 4, "v2 reads projected rows: {rows:?}");

    // 2a: refused, journalled, never placed.
    queue.enqueue(solo.id).await.unwrap();
    let failed = || async {
        let mut read = store.read().await.unwrap();
        let run = read.runs_of_task(solo.id).await.unwrap().pop();
        run.filter(|r| r.status == RunStatus::Failed)
    };
    eventually("the bypass run refused", || async {
        failed().await.is_some()
    })
    .await;
    let run = failed().await.unwrap();
    let reason = run.end_reason.clone().unwrap_or_default();
    assert!(
        reason.contains("bypass_permissions needs a container or vm"),
        "{reason}"
    );
    let events = store
        .read()
        .await
        .unwrap()
        .events_after(Some(run.id), 0, 64)
        .await
        .unwrap();
    let refused = events
        .iter()
        .find(|e| e.kind == "placement_refused_bypass_bare")
        .expect("journalled");
    let mut keys: Vec<&str> = refused
        .payload
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    keys.sort_unstable();
    assert_eq!(
        keys,
        ["exec_env", "reason", "runtime"],
        "{}",
        refused.payload
    );
    assert_eq!(refused.payload["runtime"], "fake");
    assert_eq!(refused.payload["exec_env"], "bare");
    let text = refused.payload.to_string();
    for internal in ["cred:", "login:", &h1.to_string(), &h2.to_string()] {
        assert!(!text.contains(internal), "{internal} in {text}");
    }
    for kind in ["run_placed", "session_assigned", "step_started"] {
        assert!(!events.iter().any(|e| e.kind == kind), "{kind}: {events:?}");
    }
}

/// BLO-136 case 3 (cross-workspace placement and cooldown). Workspace Main
/// runs `fake` on a key that cools; workspace Ops has no key and its own
/// host. Ops' run goes to Ops' host only (never Main's), and Main's cooling
/// key neither blocks nor marks it; Main's run waits on its own key.
#[tokio::test(flavor = "multi_thread")]
async fn queue_a_workspaces_cooling_key_never_blocks_another_workspaces_host() {
    use sun_store::{HostRepo, WorkspaceRepo};
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).await.unwrap();
    let main_task = tasks_of(&store, WORKFLOW, 1).await.remove(0);
    let main_host = add_host(&store, "main-mac", true).await;
    let key = sun_core::model::Runtime {
        id: sun_core::RuntimeId::generate(),
        workspace_id: sun_core::WorkspaceId::MAIN,
        name: "fake".into(),
        adapter: "fake".into(),
        source: RuntimeSource::Credits,
        byo_key: None,
        created_by: None,
    };
    let until = chrono::Utc::now() + chrono::Duration::minutes(10);
    // Stored before the kernel starts: the queue loads cooldowns at start.
    let mut tx = store.begin().await.unwrap();
    assert!(tx.insert_runtime(&key).await.unwrap());
    tx.set_cooldown(&Account::Credential(key.id).key(main_host), Some(until))
        .await
        .unwrap();
    tx.commit().await.unwrap();
    let link = std::sync::Arc::new(Online(std::sync::Mutex::new(vec![main_host])));
    let (engine, queue) = kernel_on(&store, link.clone(), main_host, Caps::default()).await;
    let policy = sun_core::model::SignupPolicy::default();
    let ops = engine
        .create_workspace("Ops", None, None, &policy, "user:me")
        .await
        .unwrap()
        .id;
    let ops_host = sun_core::model::Host {
        id: HostId::generate(),
        name: "ops-vps".into(),
        capabilities: serde_json::json!({
            "os": "linux", "arch": "x86_64", "cpus": 4,
            "runtimes": [{"adapter": "fake", "readiness":
                {"installed": true, "version": "1", "logged_in": true, "detail": null}}],
        }),
        enrolled_by: None,
    };
    let parsed = sun_workflow::parse(WORKFLOW).unwrap();
    let workflow = Workflow {
        id: WorkflowId::generate(),
        workspace_id: ops,
        name: parsed.name.clone(),
    };
    let ops_task = Task {
        title: "ops".into(),
        workflow_id: Some(workflow.id),
        status: TaskStatus::Backlog,
        ..sun_core::model::Task::new(ops, "")
    };
    let mut tx = store.begin().await.unwrap();
    tx.insert_host(&ops_host).await.unwrap();
    tx.link(ops, sun_store::WorkspaceItem::Host(ops_host.id))
        .await
        .unwrap();
    tx.insert_workflow(&workflow).await.unwrap();
    tx.save_workflow_version(workflow.id, WORKFLOW, &parsed.hash)
        .await
        .unwrap();
    tx.insert_task(&ops_task).await.unwrap();
    tx.commit().await.unwrap();
    link.0.lock().unwrap().push(ops_host.id);
    queue.wake();

    // Main's first: by the time Ops' run is placed, a pass has weighed it.
    let main_run = queue.enqueue(main_task.id).await.unwrap();
    queue.enqueue(ops_task.id).await.unwrap();
    eventually("Ops' run placed", || async {
        placed_on(&store, ops_task.id).await.is_some()
    })
    .await;
    assert_eq!(
        placed_on(&store, ops_task.id).await,
        Some(ops_host.id),
        "Ops' run on Ops' host, not Main's"
    );
    assert_eq!(
        run_of(&store, main_task.id).await,
        Some((main_run, RunStatus::Queued)),
        "Main's key cools: its run waits, and never goes to Ops' host"
    );
}

/// BLO-168, mixed fleets: a step's own permission mode goes only to the
/// hosts whose CLI reports it, however busy they are. A workspace where no
/// host reports it refuses the run, journalled, instead of running it in
/// another mode or leaving it queued.
#[tokio::test(flavor = "multi_thread")]
async fn queue_places_a_steps_mode_only_on_hosts_that_report_it() {
    const AUTO: &str = "name: auto\nsteps:\n  \
                        - {id: implement, type: agent, prompt: do it, permission_mode: auto}\n";
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).await.unwrap();
    let tasks = tasks_of(&store, AUTO, 3).await;
    // Reports no `permission_modes`: the legacy four, no `auto`.
    let legacy = add_host(&store, "legacy", true).await;
    let current = add_host(&store, "current", true).await;
    let link = std::sync::Arc::new(Online(std::sync::Mutex::new(vec![legacy, current])));
    let (engine, queue) = kernel_on(&store, link, legacy, Caps::default()).await;
    let with_auto = serde_json::json!({
        "os": "linux", "arch": "x86_64", "cpus": 4,
        "runtimes": [{"adapter": "fake", "readiness":
            {"installed": true, "version": "1", "logged_in": true, "detail": null},
            "permission_modes": ["default", "accept_edits", "plan", "auto", "bypass_permissions"]}],
    });
    engine
        .set_host_capabilities(current, &with_auto)
        .await
        .unwrap();
    for task in &tasks {
        queue.enqueue(task.id).await.unwrap();
    }
    for task in &tasks {
        eventually("the auto run placed", || async {
            placed_on(&store, task.id).await.is_some()
        })
        .await;
        assert_eq!(placed_on(&store, task.id).await, Some(current));
    }

    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).await.unwrap();
    let task = tasks_of(&store, AUTO, 1).await.remove(0);
    let legacy = add_host(&store, "legacy", true).await;
    let link = std::sync::Arc::new(Online(std::sync::Mutex::new(vec![legacy])));
    let (_engine, queue) = kernel_on(&store, link, legacy, Caps::default()).await;
    queue.enqueue(task.id).await.unwrap();
    let failed = || async {
        let mut read = store.read().await.unwrap();
        let run = read.runs_of_task(task.id).await.unwrap().pop();
        run.filter(|r| r.status == RunStatus::Failed)
    };
    eventually("the auto run refused", || async {
        failed().await.is_some()
    })
    .await;
    let run = failed().await.unwrap();
    let reason = run.end_reason.unwrap_or_default();
    assert!(
        reason.contains("fake doesn't offer permission mode `auto`"),
        "{reason}"
    );
    let events = store
        .read()
        .await
        .unwrap()
        .events_after(Some(run.id), 0, 32)
        .await
        .unwrap();
    let refused = events
        .iter()
        .find(|e| e.kind == "placement_refused_mode_unoffered")
        .expect("journalled");
    assert_eq!(refused.payload["runtime"], "fake");
    assert!(
        !events.iter().any(|e| e.kind == "run_placed"),
        "never placed"
    );
}

/// The latest run of `task` once it failed, if it did.
async fn failed_run(store: &Store, task: TaskId) -> Option<sun_core::model::Run> {
    let mut read = store.read().await.unwrap();
    let run = read.runs_of_task(task).await.unwrap().pop();
    run.filter(|r| r.status == RunStatus::Failed)
}

/// A mixed fleet, one run per host at a time: host `legacy` (the four
/// modes before `auto`, and the fallback) and `current`, whose `fake`
/// reports `auto`, with `count` backlog tasks of an `auto` step. Returns
/// `current`; the dir holds the store.
async fn auto_fleet(count: usize) -> (tempfile::TempDir, Store, Engine, Queue, Vec<Task>, HostId) {
    const AUTO: &str = "name: auto\nsteps:\n  \
                        - {id: implement, type: agent, prompt: do it, permission_mode: auto}\n";
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).await.unwrap();
    let tasks = tasks_of(&store, AUTO, count).await;
    let legacy = add_host(&store, "legacy", true).await;
    let current = add_host(&store, "current", true).await;
    let link = std::sync::Arc::new(Online(std::sync::Mutex::new(vec![legacy, current])));
    let one = HostCap {
        runs: Some(1),
        ..HostCap::default()
    };
    let caps = Caps {
        per_host: BTreeMap::from([("*".to_owned(), one)]),
        ..Caps::default()
    };
    let (engine, queue) = kernel_on(&store, link, legacy, caps).await;
    let with_auto = serde_json::json!({
        "os": "linux", "arch": "x86_64", "cpus": 4,
        "runtimes": [{"adapter": "fake", "readiness":
            {"installed": true, "version": "1", "logged_in": true, "detail": null},
            "permission_modes": ["default", "accept_edits", "plan", "auto", "bypass_permissions"]}],
    });
    engine
        .set_host_capabilities(current, &with_auto)
        .await
        .unwrap();
    (dir, store, engine, queue, tasks, current)
}

/// BLO-168 case 2, mixed fleet under load: a legacy host (the four modes
/// before `auto`) and one that reports `auto`, one run each at a time.
/// `auto` runs wait for the second host while the legacy one sits idle.
/// Once the second host is detached from the workspace, the waiting runs
/// are refused (journalled), not left queued; reattached, new runs place.
#[tokio::test(flavor = "multi_thread")]
async fn queue_a_mixed_fleet_refuses_auto_once_its_only_host_detaches() {
    let (_dir, store, engine, queue, tasks, current) = auto_fleet(4).await;
    for task in &tasks[..3] {
        queue.enqueue(task.id).await.unwrap();
    }
    let first = tasks[0].id;
    eventually("the first auto run placed", || async {
        placed_on(&store, first).await.is_some()
    })
    .await;
    assert_eq!(placed_on(&store, first).await, Some(current));
    // The legacy host is free the whole time; the other two still wait.
    tokio::time::sleep(Duration::from_millis(300)).await;
    for task in &tasks[1..3] {
        assert_eq!(placed_on(&store, task.id).await, None, "never on legacy");
        assert_eq!(task_status(&store, task.id).await, TaskStatus::Todo);
    }

    let host = sun_engine::WorkspaceItem::Host(current);
    engine
        .detach_from_workspace(sun_core::WorkspaceId::MAIN, host, "owner")
        .await
        .unwrap();
    // Detaching doesn't start a placement pass (BLO-174, pinned by
    // `queue_detaching_the_only_auto_host_refuses_its_waiting_runs`); the
    // next queue event does: here an unrelated plain task.
    let plain = tasks_of(&store, WORKFLOW, 1).await.remove(0);
    queue.enqueue(plain.id).await.unwrap();
    for task in &tasks[1..3] {
        eventually("the waiting auto run refused", || async {
            failed_run(&store, task.id).await.is_some()
        })
        .await;
        let run = failed_run(&store, task.id).await.unwrap();
        let reason = run.end_reason.clone().unwrap_or_default();
        assert_eq!(
            reason,
            "fake doesn't offer permission mode `auto`; \
             it offers default, accept_edits, plan, bypass_permissions"
        );
        let events = store
            .read()
            .await
            .unwrap()
            .events_after(Some(run.id), 0, 64)
            .await
            .unwrap();
        let refused = events
            .iter()
            .find(|e| e.kind == "placement_refused_mode_unoffered")
            .expect("journalled");
        assert_eq!(refused.payload["runtime"], "fake");
        assert_eq!(refused.payload["exec_env"], "bare");
        assert!(!events.iter().any(|e| e.kind == "run_placed"));
    }

    // Reattached: a new auto run places on it again (the first run left).
    let (running, _) = run_of(&store, first).await.unwrap();
    engine.cancel(running).await.unwrap();
    {
        use sun_store::WorkspaceRepo;
        let mut tx = store.begin().await.unwrap();
        tx.link(sun_core::WorkspaceId::MAIN, host).await.unwrap();
        tx.commit().await.unwrap();
    }
    let again = tasks[3].id;
    queue.enqueue(again).await.unwrap();
    eventually("the new auto run placed", || async {
        placed_on(&store, again).await.is_some()
    })
    .await;
    assert_eq!(placed_on(&store, again).await, Some(current));
}

/// A member the policy doesn't allow to bypass runs a Codex bypass step on
/// a host whose Codex reports what `Codex::permission_modes` does without
/// `--approve-for-me`. Waits until the run failed; the dir holds the store.
async fn capped_codex_refusal() -> (tempfile::TempDir, Store, Engine, TaskId) {
    use sun_store::MemberRepo;
    const BYPASS: &str = "name: bypass\nruntime: {chain: [codex]}\nsteps:\n  \
                          - {id: implement, type: agent, prompt: do it, permission_mode: bypass_permissions}\n";
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).await.unwrap();
    let task = tasks_of(&store, BYPASS, 1).await.remove(0);
    let mut tx = store.begin().await.unwrap();
    let ana = tx
        .upsert_member("local", "ana@sun.test", "ana", Some("ana@sun.test"), false)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    let host = add_host(&store, "bare", true).await;
    let link = std::sync::Arc::new(Online(std::sync::Mutex::new(vec![host])));
    let (engine, queue) = kernel_on(&store, link, host, Caps::default()).await;
    let codex = serde_json::json!({
        "os": "linux", "arch": "x86_64", "cpus": 4,
        "runtimes": [{"adapter": "codex", "readiness":
            {"installed": true, "version": "1", "logged_in": true, "detail": null},
            "permission_modes": ["default", "plan", "bypass_permissions"]}],
    });
    engine.set_host_capabilities(host, &codex).await.unwrap();
    let by = sun_engine::Actor {
        member_id: Some(ana.id),
        name: "user:ana".into(),
    };
    queue.enqueue_by(task.id, Some(&by)).await.unwrap();
    eventually("the capped codex run refused", || async {
        failed_run(&store, task.id).await.is_some()
    })
    .await;
    (dir, store, engine, task.id)
}

/// BLO-168 case 4 (Rimuru's decision): capped to `accept_edits`, which
/// Codex has no exact equivalent for, the member's bypass run is refused at
/// placement, never swapped to `plan` or `workspace-write`. Its thread
/// saying why is [`queue_a_refused_capped_codex_run_says_why_on_its_thread`]
/// (BLO-175).
#[tokio::test(flavor = "multi_thread")]
async fn queue_refuses_a_capped_members_codex_bypass_run() {
    let (_dir, store, _engine, task) = capped_codex_refusal().await;
    let run = failed_run(&store, task).await.unwrap();
    let why = "codex doesn't offer permission mode `accept_edits`; \
               it offers default, plan, bypass_permissions";
    assert_eq!(run.end_reason.as_deref(), Some(why));
    let events = store
        .read()
        .await
        .unwrap()
        .events_after(Some(run.id), 0, 64)
        .await
        .unwrap();
    let refused = events
        .iter()
        .find(|e| e.kind == "placement_refused_mode_unoffered")
        .unwrap_or_else(|| panic!("not journalled: {events:?}"));
    assert_eq!(refused.payload["runtime"], "codex");
    assert_eq!(refused.payload["reason"], why);
    let kinds: Vec<&str> = events.iter().map(|e| e.kind.as_str()).collect();
    assert!(!kinds.contains(&"run_placed"), "never placed: {kinds:?}");
    assert!(!kinds.contains(&"step_status"), "no step ran: {kinds:?}");
}

/// BLO-168 case 4, the visible half: the task's thread says why the run
/// failed, and that the member's bypass was capped (BLO-175): the cap line
/// of a session start is never reached, so the refusal says both.
#[tokio::test(flavor = "multi_thread")]
async fn queue_a_refused_capped_codex_run_says_why_on_its_thread() {
    let (_dir, _store, engine, task) = capped_codex_refusal().await;
    let activity = engine.activity(task, 0).await.unwrap();
    let thread: Vec<String> = activity.iter().map(|a| a.body.to_string()).collect();
    let why = "codex doesn't offer permission mode `accept_edits`";
    assert!(
        thread.iter().any(|line| line.contains(why)),
        "thread doesn't say why: {thread:#?}"
    );
    assert!(
        thread
            .iter()
            .any(|line| line.contains("bypass permissions is not enabled for ana")),
        "thread doesn't say the bypass was capped: {thread:#?}"
    );
}

/// BLO-168 case 2, as specified: detaching the only host that reports
/// `auto` refuses the runs waiting for it, with no other queue event.
/// RED (BLO-174): `Engine::detach_from_workspace` starts no placement
/// pass, so the runs stay queued until something else wakes the queue.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "BLO-174: detaching a host starts no placement pass"]
async fn queue_detaching_the_only_auto_host_refuses_its_waiting_runs() {
    let (_dir, store, engine, queue, tasks, current) = auto_fleet(2).await;
    for task in &tasks {
        queue.enqueue(task.id).await.unwrap();
    }
    eventually("the first auto run placed", || async {
        placed_on(&store, tasks[0].id).await.is_some()
    })
    .await;
    // Let the passes the placement itself causes finish first.
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(task_status(&store, tasks[1].id).await, TaskStatus::Todo);
    let host = sun_engine::WorkspaceItem::Host(current);
    engine
        .detach_from_workspace(sun_core::WorkspaceId::MAIN, host, "owner")
        .await
        .unwrap();
    eventually("the waiting auto run refused", || async {
        failed_run(&store, tasks[1].id).await.is_some()
    })
    .await;
}
