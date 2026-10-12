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
use sun_core::model::{RuntimeSource, Task, Workflow};
use sun_core::{
    BoxStream, Error, HostEvent, HostId, HostLink, ModCall, ModLink, Result, RunId, RunStatus,
    SessionHandle, SessionId, SessionSpec, Steer, StepKey, StepStatus, StopMode, TaskId,
    TaskStatus, WorkflowId,
};
use sun_engine::{
    Caps, Engine, EngineConfig, HostCap, MarketPolicy, Principal, Queue, QueueConfig, Slot,
    TaskLease, WatchdogConfig,
};
use sun_store::{EventLog, NewEvent, RunRepo, RuntimeRepo, Store, TaskRepo, WorkflowRepo};

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
    let cooling = serde_json::json!({ "runtime": "fake", "until": reset_at });
    let mut tx = store.begin().await.unwrap();
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
    let cooling = serde_json::json!({ "runtime": "fake", "until": until });
    let mut tx = store.begin().await.unwrap();
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

    // The cooldown lands only in `runtime_state`, under more events than the
    // queue's receiver holds.
    let mut tx = store.begin().await.unwrap();
    let fake = tx
        .upsert_runtime(
            sun_core::WorkspaceId::MAIN,
            "fake",
            "fake",
            RuntimeSource::ByoRuntime,
        )
        .await
        .unwrap();
    let until = chrono::Utc::now() + chrono::Duration::minutes(10);
    tx.set_cooldown(fake.id, Some(until)).await.unwrap();
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
        // Lagging only with the cooldown in `runtime_state` too, as failover
        // writes it: an event-only cooldown the receiver drops is lost.
        let in_store = random(2) == 1;
        let fillers = if in_store { random(3) * 700 } else { 0 };
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
        if in_store {
            let fake = tx
                .upsert_runtime(
                    sun_core::WorkspaceId::MAIN,
                    "fake",
                    "fake",
                    RuntimeSource::ByoRuntime,
                )
                .await
                .unwrap();
            tx.set_cooldown(fake.id, Some(until)).await.unwrap();
        }
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
                "{what}: started on a cooling runtime (in store: {in_store}, fillers: {fillers})"
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
