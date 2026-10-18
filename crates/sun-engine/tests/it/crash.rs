//! Durability checks for the executor.
//!
//! The main test spawns this test binary as a child process (`crash_child`)
//! that runs the engine over a fake host and fake mods, `kill -9`s it at
//! random points 50 times, restarts it, and checks that no succeeded step
//! ever ran again and no side effect was applied twice. Reproduce a failure
//! with `SUN_CRASH_SEED=<seed printed on stderr>`.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use serde_json::{Value, json};
use sun_core::model::{Budget, OutboxStatus, Run, Step, Task, Workflow};
use sun_core::{
    BoxStream, HostEvent, HostId, HostLink, ModCall, ModLink, Result, Role, RunId, RunStatus,
    SessionHandle, SessionId, SessionSpec, Steer, StepStatus, StopMode, SunEvent, TaskStatus,
    WorkflowId,
};
use sun_engine::{Engine, EngineConfig, Principal, WatchdogConfig};
use sun_store::{EventLog, OutboxRepo, RunRepo, Store, TaskRepo, WorkflowRepo};
use tokio::sync::mpsc;

const CHILD_DIR: &str = "SUN_CRASH_DATA_DIR";
const KILLS: usize = 50;
const RUNS: usize = 30;
/// Side effects per run of [`CRASH_WORKFLOW`]: `pr` + two `tell` targets.
const EFFECTS_PER_RUN: usize = 3;

/// Every engine-driven step type plus two agent steps, with a fan-out.
const CRASH_WORKFLOW: &str = r#"
name: crash
steps:
  - id: implement
    type: agent
    prompt: implement it
  - id: pr
    type: action
    use: github.open_pr
  - id: tell
    type: notify
    to: ["slack:#eng", "slack:#ops"]
    message: shipped
    needs: [implement]
  - id: pause
    type: wait
    for: 1s
    needs: [pr, tell]
  - id: review
    type: agent
    prompt: review it
  - id: approve
    type: approval
    who: [role:admin]
"#;

// ---------------------------------------------------------------- fakes

/// Agent sessions living in this process, so they die with it like a local
/// host under a kernel crash. Every start is appended to `starts.log` as
/// `<run_id> <step_key> <session_id>`.
struct FakeHost {
    log: PathBuf,
    /// Sessions do nothing until this is set.
    go: Arc<AtomicBool>,
    alive: Arc<Mutex<HashSet<SessionId>>>,
    sink: Arc<Mutex<Option<mpsc::UnboundedSender<HostEvent>>>>,
}

impl FakeHost {
    fn new(log: PathBuf, go: bool) -> Self {
        Self {
            log,
            go: Arc::new(AtomicBool::new(go)),
            alive: Arc::default(),
            sink: Arc::default(),
        }
    }
}

#[async_trait]
impl HostLink for FakeHost {
    async fn prepare_worktree(
        &self,
        _: HostId,
        _: sun_core::host_protocol::Worktree,
    ) -> Result<PathBuf> {
        Ok(std::env::temp_dir())
    }

    async fn start_session(
        &self,
        _host: HostId,
        _adapter: &str,
        spec: SessionSpec,
    ) -> Result<SessionHandle> {
        let id = spec.session_id;
        append_line(
            &self.log,
            &format!("{} {} {id}", spec.run_id, spec.step_key),
        );
        self.alive.lock().unwrap().insert(id);
        let (go, alive, sink) = (self.go.clone(), self.alive.clone(), self.sink.clone());
        tokio::spawn(async move {
            while !go.load(Ordering::SeqCst) {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            let script = [
                SunEvent::Text {
                    role: Role::Assistant,
                    text: "working".into(),
                },
                SunEvent::ToolCall {
                    name: "Bash".into(),
                    args_hash: 1,
                    summary: "cargo test".into(),
                },
                SunEvent::FileChanged {
                    path: "src/lib.rs".into(),
                },
                SunEvent::DoneClaimed {
                    summary: "done".into(),
                },
                SunEvent::Exited { code: Some(0) },
            ];
            for (seq, event) in (1..).zip(script) {
                tokio::time::sleep(Duration::from_millis(8)).await;
                if !alive.lock().unwrap().contains(&id) {
                    return;
                }
                if let Some(sink) = sink.lock().unwrap().as_ref() {
                    let _ = sink.send(HostEvent {
                        session_id: id,
                        seq,
                        event,
                    });
                }
            }
            alive.lock().unwrap().remove(&id);
        });
        Ok(SessionHandle {
            session_id: id,
            native_id: None,
            pid: None,
        })
    }

    async fn steer(&self, _session: &SessionHandle, _msg: Steer) -> Result<()> {
        Ok(())
    }

    async fn stop_session(&self, session: &SessionHandle, _how: StopMode) -> Result<()> {
        self.alive.lock().unwrap().remove(&session.session_id);
        Ok(())
    }

    async fn is_alive(&self, session: &SessionHandle) -> Result<bool> {
        Ok(self.alive.lock().unwrap().contains(&session.session_id))
    }

    fn events(&self) -> BoxStream<'static, HostEvent> {
        let (tx, rx) = mpsc::unbounded_channel();
        *self.sink.lock().unwrap() = Some(tx);
        Box::pin(Receiver(rx))
    }

    async fn ack(&self, _session: SessionId, _seq: u64) -> Result<()> {
        Ok(())
    }
}

struct Receiver<T>(mpsc::UnboundedReceiver<T>);

impl<T> futures_core::Stream for Receiver<T> {
    type Item = T;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<T>> {
        self.0.poll_recv(cx)
    }
}

/// A plugin that applies a side effect by appending its idem key to
/// `effects.log` (no dedupe of its own, so a double delivery shows up), and
/// answers `reconcile` from that log.
struct FakeMods {
    log: PathBuf,
    /// Never answer (to force timeouts).
    hang: AtomicBool,
    /// Refuse this many more deliveries definitively.
    refuse: AtomicUsize,
    calls: AtomicUsize,
}

impl FakeMods {
    fn new(log: PathBuf, hang: bool) -> Self {
        Self {
            log,
            hang: AtomicBool::new(hang),
            refuse: AtomicUsize::new(0),
            calls: AtomicUsize::new(0),
        }
    }
}

#[async_trait]
impl ModLink for FakeMods {
    async fn call(&self, call: ModCall) -> Result<Value> {
        let key = call.idem_key.expect("outbox calls carry an idem key");
        if call.method != "reconcile" {
            self.calls.fetch_add(1, Ordering::SeqCst);
        }
        if self.hang.load(Ordering::SeqCst) {
            std::future::pending::<()>().await;
        }
        if call.method != "reconcile"
            && self
                .refuse
                .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1))
                .is_ok()
        {
            return Err(sun_core::Error::InvalidInput("refused".into()));
        }
        if call.method == "reconcile" {
            let delivered = read_lines(&self.log).contains(&key.as_str().to_owned());
            return Ok(json!({ "delivered": delivered, "result": { "ok": true } }));
        }
        // Crash windows on both sides of applying the effect.
        tokio::time::sleep(Duration::from_millis(2)).await;
        append_line(&self.log, key.as_str());
        tokio::time::sleep(Duration::from_millis(2)).await;
        Ok(json!({ "ok": true }))
    }
}

fn append_line(path: &Path, line: &str) {
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .unwrap();
    // One write: a kill -9 between `line` and its newline would glue two
    // lines together.
    file.write_all(format!("{line}\n").as_bytes()).unwrap();
}

fn read_lines(path: &Path) -> Vec<String> {
    std::fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .map(str::to_owned)
        .collect()
}

// ---------------------------------------------------------------- helpers

fn config(delivery_timeout: Duration) -> EngineConfig {
    EngineConfig {
        allow_bypass: false,
        host: HostId::generate(),
        adapter: "fake".into(),
        delivery_timeout,
        watchdog: WatchdogConfig::default(),
    }
}

/// Commits a workflow (v1 = `yaml`) and `runs` queued runs of it.
async fn seed(store: &Store, yaml: &str, runs: usize) -> Vec<RunId> {
    let parsed = sun_workflow::parse(yaml).unwrap();
    let workflow = Workflow {
        id: WorkflowId::generate(),
        workspace_id: sun_core::WorkspaceId::MAIN,
        name: parsed.name.clone(),
    };
    let mut tx = store.begin().await.unwrap();
    tx.insert_workflow(&workflow).await.unwrap();
    tx.save_workflow_version(workflow.id, yaml, &parsed.hash)
        .await
        .unwrap();
    let mut ids = Vec::new();
    for _ in 0..runs {
        let task = Task {
            title: "crash".into(),
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
        tx.insert_task(&task).await.unwrap();
        tx.insert_run(&run).await.unwrap();
        ids.push(run.id);
    }
    tx.commit().await.unwrap();
    ids
}

async fn steps(store: &Store, run: RunId) -> Vec<Step> {
    store.read().await.unwrap().steps_of_run(run).await.unwrap()
}

async fn run_status(store: &Store, run: RunId) -> RunStatus {
    store
        .read()
        .await
        .unwrap()
        .get_run(run)
        .await
        .unwrap()
        .status
}

async fn eventually<F, Fut>(what: &str, mut check: F)
where
    F: FnMut() -> Fut,
    Fut: Future<Output = bool>,
{
    let deadline = Instant::now() + Duration::from_secs(20);
    while !check().await {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// Plays the queue and the approver: starts one queued run per pass (so the
/// work spreads over many child lives), approves waiting steps. True once
/// no run is active.
async fn drive(store: &Store, engine: &Engine) -> bool {
    let runs = store.read().await.unwrap().active_runs().await.unwrap();
    if let Some(run) = runs.iter().find(|r| r.status == RunStatus::Queued) {
        engine.start_run(run.id).await.unwrap();
    }
    for run in &runs {
        for step in steps(store, run.id).await {
            if step.status == StepStatus::WaitingApproval {
                let by = Principal {
                    user: "crash-test".into(),
                    roles: vec!["admin".into()],
                };
                engine
                    .decide(run.id, step.step_key, true, by)
                    .await
                    .unwrap();
            }
        }
    }
    runs.is_empty()
}

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap()
}

// ------------------------------------------------------------ crash test

/// The process the main test kills: engine + fakes + queue/approver loop.
#[test]
#[ignore = "child process of `kill_9_fifty_times_never_reexecutes`"]
fn crash_child() {
    let Ok(dir) = std::env::var(CHILD_DIR) else {
        return;
    };
    let dir = PathBuf::from(dir);
    runtime().block_on(async {
        let store = Store::open(&dir).await.unwrap();
        let host = Arc::new(FakeHost::new(dir.join("starts.log"), true));
        let mods = Arc::new(FakeMods::new(dir.join("effects.log"), false));
        let config = config(Duration::from_secs(5));
        let (engine, task) = Engine::start(store.clone(), host, mods, config)
            .await
            .unwrap();
        while !drive(&store, &engine).await {
            assert!(!task.is_finished(), "engine task stopped");
            tokio::time::sleep(Duration::from_millis(80)).await;
        }
    });
}

/// `(run, step_key)` → number of agent sessions started for it.
fn starts(dir: &Path) -> HashMap<(RunId, String), usize> {
    let mut out = HashMap::new();
    for line in read_lines(&dir.join("starts.log")) {
        let mut parts = line.split(' ');
        let run: RunId = parts.next().unwrap().parse().unwrap();
        let key = parts.next().unwrap().to_owned();
        *out.entry((run, key)).or_insert(0) += 1;
    }
    out
}

async fn succeeded(store: &Store, runs: &[RunId]) -> HashSet<(RunId, String)> {
    let mut out = HashSet::new();
    for &run in runs {
        for step in steps(store, run).await {
            if step.status == StepStatus::Succeeded {
                out.insert((run, step.step_key.to_string()));
            }
        }
    }
    out
}

fn spawn_child(dir: &Path) -> Child {
    Command::new(std::env::current_exe().unwrap())
        .args(["crash::crash_child", "--exact", "--ignored", "--nocapture"])
        .env(CHILD_DIR, dir)
        .stdout(Stdio::null())
        .spawn()
        .unwrap()
}

/// xorshift64; the seed is printed so a failure can be replayed.
struct Rng(u64);

impl Rng {
    fn below(&mut self, n: u64) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0 % n
    }
}

#[test]
fn kill_9_fifty_times_never_reexecutes() {
    let dir = tempfile::tempdir().unwrap();
    let dir = dir.path();
    let rt = runtime();
    let store = rt.block_on(Store::open(dir)).unwrap();
    let runs = rt.block_on(seed(&store, CRASH_WORKFLOW, RUNS));
    let seed = std::env::var("SUN_CRASH_SEED")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or_else(|| {
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            u64::try_from(nanos % u128::from(u64::MAX)).unwrap() | 1
        });
    eprintln!("crash test seed: {seed}");
    let mut rng = Rng(seed);
    let mut progressed = 0;
    let total = RUNS * sun_workflow::parse(CRASH_WORKFLOW).unwrap().steps.len();

    for round in 0..=KILLS {
        let done_before = rt.block_on(succeeded(&store, &runs));
        let starts_before = starts(dir);
        let mut child = spawn_child(dir);
        if round < KILLS {
            // Odd rounds wait for this life's first progress, so kills still
            // land mid-work when a loaded machine slows the child's start
            // (the one unexplained failure seen: "too few kills mid-work").
            // Even rounds keep a blind delay that also hits start-up/resume.
            if round % 2 == 1 && done_before.len() < total {
                let deadline = Instant::now() + Duration::from_secs(10);
                while Instant::now() < deadline
                    && rt.block_on(succeeded(&store, &runs)).len() == done_before.len()
                {
                    std::thread::sleep(Duration::from_millis(5));
                }
            }
            std::thread::sleep(Duration::from_millis(20 + rng.below(300)));
            child.kill().unwrap(); // SIGKILL on unix
            child.wait().unwrap();
        } else {
            let deadline = Instant::now() + Duration::from_secs(60);
            loop {
                if let Some(status) = child.try_wait().unwrap() {
                    assert!(status.success(), "final child failed: {status}");
                    break;
                }
                if Instant::now() > deadline {
                    child.kill().unwrap();
                    panic!("final child did not finish all runs");
                }
                std::thread::sleep(Duration::from_millis(20));
            }
        }
        // Succeeded steps stay succeeded and never get another session.
        let done_after = rt.block_on(succeeded(&store, &runs));
        // A kill counts as mid-work when it cut a life that made progress.
        if round < KILLS && done_after.len() > done_before.len() {
            progressed += 1;
        }
        let starts_after = starts(dir);
        for key in &done_before {
            assert!(
                done_after.contains(key),
                "round {round}: {key:?} left succeeded"
            );
            assert_eq!(
                starts_after.get(key),
                starts_before.get(key),
                "round {round}: succeeded step {key:?} was started again"
            );
        }
    }

    rt.block_on(async {
        let workflow = sun_workflow::parse(CRASH_WORKFLOW).unwrap();
        for &run in &runs {
            assert_eq!(run_status(&store, run).await, RunStatus::Succeeded);
            let rows = steps(&store, run).await;
            assert_eq!(rows.len(), workflow.steps.len(), "one attempt per step");
            assert!(rows.iter().all(|s| s.status == StepStatus::Succeeded));
        }
        let mut read = store.read().await.unwrap();
        for status in [
            OutboxStatus::Pending,
            OutboxStatus::Unknown,
            OutboxStatus::Failed,
        ] {
            let left = read.outbox_with_status(status, 100).await.unwrap();
            assert!(left.is_empty(), "outbox rows left {status}: {left:?}");
        }
        let delivered: HashSet<String> = read
            .outbox_with_status(OutboxStatus::Delivered, 1000)
            .await
            .unwrap()
            .into_iter()
            .map(|item| item.idem_key.as_str().to_owned())
            .collect();
        assert_eq!(delivered.len(), RUNS * EFFECTS_PER_RUN);
        let effects = read_lines(&dir.join("effects.log"));
        let unique: HashSet<String> = effects.iter().cloned().collect();
        assert_eq!(
            unique.len(),
            effects.len(),
            "a side effect was applied twice"
        );
        assert_eq!(
            unique, delivered,
            "every delivered row applied exactly once"
        );
    });
    let agent_steps = RUNS * 2;
    let total_starts: usize = starts(dir).values().sum();
    eprintln!(
        "{KILLS} kills ({progressed} mid-work): {total_starts} sessions for {agent_steps} agent steps"
    );
    assert!(progressed >= 10, "too few kills landed mid-work");
}

// ----------------------------------------------------- in-process checks

const ONE_AGENT: &str = "
name: one
steps:
  - id: implement
    type: agent
    prompt: go
";

const ONE_ACTION: &str = "
name: act
steps:
  - id: pr
    type: action
    use: github.open_pr
";

/// A session still alive on its host after a kernel restart is re-attached,
/// not started again.
#[tokio::test(flavor = "multi_thread")]
async fn resume_reattaches_a_live_session() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).await.unwrap();
    let run = seed(&store, ONE_AGENT, 1).await[0];
    let log = dir.path().join("starts.log");
    let host = Arc::new(FakeHost::new(log.clone(), false));
    let mods = Arc::new(FakeMods::new(dir.path().join("effects.log"), false));
    let config = config(Duration::from_secs(5));

    let (engine, task) = Engine::start(store.clone(), host.clone(), mods.clone(), config.clone())
        .await
        .unwrap();
    engine.start_run(run).await.unwrap();
    let store_ref = &store;
    eventually("step running", || async move {
        steps(store_ref, run).await[0].status == StepStatus::Running
    })
    .await;
    task.abort();
    let _ = task.await;

    let (_engine, _task) = Engine::start(store.clone(), host.clone(), mods, config)
        .await
        .unwrap();
    host.go.store(true, Ordering::SeqCst);
    eventually("run succeeded", || async move {
        run_status(store_ref, run).await == RunStatus::Succeeded
    })
    .await;
    assert_eq!(
        read_lines(&log).len(),
        1,
        "the live session was re-attached"
    );
}

/// A delivery that times out is `unknown` and is never sent again, not even
/// after a restart, while `reconcile` can't answer.
#[tokio::test(flavor = "multi_thread")]
async fn delivery_timeout_is_unknown_and_never_resent() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).await.unwrap();
    let run = seed(&store, ONE_ACTION, 1).await[0];
    let host = Arc::new(FakeHost::new(dir.path().join("starts.log"), true));
    let mods = Arc::new(FakeMods::new(dir.path().join("effects.log"), true));
    let config = config(Duration::from_millis(50));
    let store_ref = &store;
    let unknown = || async move {
        let mut read = store_ref.read().await.unwrap();
        read.outbox_with_status(OutboxStatus::Unknown, 10)
            .await
            .unwrap()
            .len()
            == 1
    };

    let (engine, task) = Engine::start(store.clone(), host.clone(), mods.clone(), config.clone())
        .await
        .unwrap();
    engine.start_run(run).await.unwrap();
    eventually("row unknown", unknown).await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    task.abort();
    let _ = task.await;

    let (_engine, _task) = Engine::start(store.clone(), host, mods.clone(), config)
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(mods.calls.load(Ordering::SeqCst), 1, "never resent");
    assert!(unknown().await, "still unknown");
    assert_eq!(steps(&store, run).await[0].status, StepStatus::Running);
    assert_eq!(run_status(&store, run).await, RunStatus::Running);
}

/// Agent work, then a PR that fails once.
const BUILD_THEN_PR: &str = "
name: retry
steps:
  - id: implement
    type: agent
    prompt: go
  - id: pr
    type: action
    use: github.open_pr
";

/// A human retries the failed `pr` step of a failed run: the run reopens,
/// `implement` never runs again, `pr` is sent again under its key and the
/// run succeeds. A `retry:` policy does the same without the human.
#[tokio::test(flavor = "multi_thread")]
async fn step_retry_keeps_completed_steps() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).await.unwrap();
    let run = seed(&store, BUILD_THEN_PR, 1).await[0];
    let starts = dir.path().join("starts.log");
    let host = Arc::new(FakeHost::new(starts.clone(), true));
    let mods = Arc::new(FakeMods::new(dir.path().join("effects.log"), false));
    mods.refuse.store(1, Ordering::SeqCst);
    let (engine, _task) = Engine::start(
        store.clone(),
        host.clone(),
        mods.clone(),
        config(Duration::from_secs(5)),
    )
    .await
    .unwrap();
    engine.start_run(run).await.unwrap();
    let store_ref = &store;
    eventually("run failed", || async move {
        run_status(store_ref, run).await == RunStatus::Failed
    })
    .await;
    let pr = sun_core::StepKey::new("pr").unwrap();
    let implement = sun_core::StepKey::new("implement").unwrap();
    // Only a step that ended badly can be retried.
    let err = engine
        .retry_step(
            run,
            implement.clone(),
            sun_engine::Actor {
                member_id: None,
                name: "user:ana".into(),
            },
        )
        .await
        .unwrap_err();
    assert!(err.to_string().contains("succeeded"), "{err}");

    engine
        .retry_step(
            run,
            pr.clone(),
            sun_engine::Actor {
                member_id: None,
                name: "user:ana".into(),
            },
        )
        .await
        .unwrap();
    eventually("run succeeded", || async move {
        run_status(store_ref, run).await == RunStatus::Succeeded
    })
    .await;
    assert_eq!(read_lines(&starts).len(), 1, "implement ran once");
    let rows = steps(&store, run).await;
    let attempts = |key: &sun_core::StepKey| {
        rows.iter()
            .filter(|s| s.step_key == *key)
            .map(|s| (s.attempt, s.status))
            .collect::<Vec<_>>()
    };
    assert_eq!(attempts(&implement), [(1, StepStatus::Succeeded)]);
    assert_eq!(
        attempts(&pr),
        [(1, StepStatus::Failed), (2, StepStatus::Succeeded)]
    );
    assert_eq!(mods.calls.load(Ordering::SeqCst), 2, "sent again once");
    let task = store.read().await.unwrap().tasks().await.unwrap().remove(0);
    // No approval step: a person reviews the result and marks it Done.
    assert_eq!(task.status, TaskStatus::NeedsYou);

    // The same failure under `retry: {max: 2}` retries on its own.
    let policy = BUILD_THEN_PR.replace(
        "use: github.open_pr",
        "use: github.open_pr\n    retry: {max: 2}",
    );
    let run = seed(&store, &policy, 1).await[0];
    mods.refuse.store(1, Ordering::SeqCst);
    engine.start_run(run).await.unwrap();
    eventually("policy run succeeded", || async move {
        run_status(store_ref, run).await == RunStatus::Succeeded
    })
    .await;
    assert_eq!(
        steps(&store, run).await.len(),
        3,
        "implement + two pr attempts"
    );
}

/// An `unknown` delivery waits for a human: `resend` sends it exactly once
/// more, a second decision is refused.
#[tokio::test(flavor = "multi_thread")]
async fn unknown_delivery_resolves_to_exactly_one_resend() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).await.unwrap();
    let run = seed(&store, ONE_ACTION, 1).await[0];
    let host = Arc::new(FakeHost::new(dir.path().join("starts.log"), true));
    let effects = dir.path().join("effects.log");
    let mods = Arc::new(FakeMods::new(effects.clone(), true));
    // The first call hangs until this timeout; the resend must never hit it.
    let (engine, _task) = Engine::start(
        store.clone(),
        host,
        mods.clone(),
        config(Duration::from_secs(1)),
    )
    .await
    .unwrap();
    engine.start_run(run).await.unwrap();
    // Settled `unknown` (timed out), not merely marked so before the call:
    // until then a decision is refused and `hang` must not change yet.
    let store_ref = &store;
    eventually("delivery timed out", || async move {
        let mut read = store_ref.read().await.unwrap();
        let events = read.events_after(Some(run), 0, 10_000).await.unwrap();
        events.iter().any(|e| e.kind == "outbox_unknown")
    })
    .await;
    let row = engine
        .outbox(OutboxStatus::Unknown)
        .await
        .unwrap()
        .remove(0);
    mods.hang.store(false, Ordering::SeqCst);
    engine
        .resolve_outbox(row.id, sun_engine::Resolution::Resend, "ana".into())
        .await
        .unwrap();
    let again = engine
        .resolve_outbox(row.id, sun_engine::Resolution::Resend, "ana".into())
        .await;
    assert!(again.is_err(), "a second decision is refused");
    eventually("run succeeded", || async move {
        run_status(store_ref, run).await == RunStatus::Succeeded
    })
    .await;
    assert_eq!(
        mods.calls.load(Ordering::SeqCst),
        2,
        "one timeout, one resend"
    );
    assert_eq!(read_lines(&effects), [row.idem_key.as_str().to_owned()]);
    let delivered = engine.outbox(OutboxStatus::Delivered).await.unwrap();
    assert_eq!(delivered.len(), 1);
}

/// A run that failed on `pr` while `implement` was still working: retrying
/// `pr` reopens the run and picks `implement` up again through a hand-off
/// (attempt 2, same worktree) instead of from scratch.
#[tokio::test(flavor = "multi_thread")]
async fn step_retry_hands_a_cancelled_agent_off() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).await.unwrap();
    let parallel =
        BUILD_THEN_PR.replace("use: github.open_pr", "use: github.open_pr\n    needs: []");
    let run = seed(&store, &parallel, 1).await[0];
    let starts = dir.path().join("starts.log");
    let host = Arc::new(FakeHost::new(starts.clone(), false));
    let mods = Arc::new(FakeMods::new(dir.path().join("effects.log"), false));
    mods.refuse.store(1, Ordering::SeqCst);
    let config = config(Duration::from_secs(5));
    let (engine, _task) = Engine::start(store.clone(), host.clone(), mods, config)
        .await
        .unwrap();
    engine.start_run(run).await.unwrap();
    let store_ref = &store;
    eventually("run failed", || async move {
        run_status(store_ref, run).await == RunStatus::Failed
    })
    .await;
    host.go.store(true, Ordering::SeqCst);
    let pr = sun_core::StepKey::new("pr").unwrap();
    engine
        .retry_step(
            run,
            pr,
            sun_engine::Actor {
                member_id: None,
                name: "user:ana".into(),
            },
        )
        .await
        .unwrap();
    eventually("run succeeded", || async move {
        run_status(store_ref, run).await == RunStatus::Succeeded
    })
    .await;
    assert_eq!(read_lines(&starts).len(), 2, "one session per attempt");
    let events = store
        .read()
        .await
        .unwrap()
        .events_after(Some(run), 0, 10_000)
        .await
        .unwrap();
    let handoff = events.iter().find(|e| e.kind == "handoff").unwrap();
    assert_eq!(handoff.payload["reason"], "retry");
    assert_eq!(events.iter().filter(|e| e.kind == "step_retry").count(), 2);
}
