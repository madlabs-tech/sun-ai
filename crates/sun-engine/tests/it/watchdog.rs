//! Watchdog end to end: the engine runs one agent step on the scripted
//! fake runtime (`sun-fake-runtime`) through an in-process host,
//! and every run must end in an explicit state with a reason.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use crate::support;

use std::collections::HashMap;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::{Arc, Mutex, OnceLock};
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use serde_json::Value;
use sun_core::model::{Budget, Run, Task, Workflow};
use sun_core::{
    BoxStream, Error, HostEvent, HostId, HostLink, ModCall, ModLink, Result, RunId, RunStatus,
    RuntimeAdapter, SessionHandle, SessionId, SessionSpec, StateMachine, Steer, StopMode,
    TaskStatus, WorkflowId,
};
use sun_engine::{DecisionClient, Engine, EngineConfig, StallPolicy, WatchdogConfig};
use sun_runtime::FakeRuntime;
use sun_store::{EventLog, RunRepo, Store, TaskRepo, WorkflowRepo};
use tokio::sync::mpsc;

const WORKFLOW: &str = "
name: watched
steps:
  - id: build
    type: agent
    prompt: build it
";

const TALK: &str = r#"{"kind":"text","role":"assistant","text":"on it"}"#;
const DONE: &str = r#"{"kind":"done_claimed","summary":"built"}"#;
const EXIT: &str = r#"{"kind":"exited","code":0}"#;
const HANG: &str = r#"{"delay_ms":60000,"kind":"done_claimed","summary":"late"}"#;
const BASH: &str = r#"{"kind":"tool_call","name":"Bash","args_hash":7,"summary":"cargo test"}"#;
const SPEND: &str = r#"{"kind":"usage","input":10,"output":10,"cost_usd":0.6,"context_used":null}"#;

/// Builds `sun-fake-runtime` once (a no-op when it is fresh) and returns its path.
fn fake_binary() -> PathBuf {
    static BINARY: OnceLock<PathBuf> = OnceLock::new();
    BINARY
        .get_or_init(|| {
            let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
            let status = std::process::Command::new(cargo)
                .args([
                    "build",
                    "-q",
                    "-p",
                    "sun-runtime",
                    "--bin",
                    "sun-fake-runtime",
                ])
                .status()
                .unwrap();
            assert!(status.success(), "building sun-fake-runtime failed");
            // target/<profile>/deps/it-<hash> → target/<profile>/
            let exe = std::env::current_exe().unwrap();
            let binary = exe
                .parent()
                .unwrap()
                .parent()
                .unwrap()
                .join("sun-fake-runtime");
            // macOS scans a freshly built binary on its first exec (up to
            // ~1 s): pay that here, not inside a session's stall window.
            let _ = std::process::Command::new(&binary).output();
            binary
        })
        .clone()
}

/// An in-process host: the n-th session runs the n-th fake runtime (the last
/// one repeats), and every session's events are forwarded with a host seq.
struct LocalHost {
    runtimes: Vec<FakeRuntime>,
    owners: Mutex<HashMap<SessionId, usize>>,
    envelopes: Mutex<Vec<String>>,
    sink: mpsc::UnboundedSender<HostEvent>,
    source: Mutex<Option<mpsc::UnboundedReceiver<HostEvent>>>,
}

impl LocalHost {
    fn runtime(&self, session: &SessionHandle) -> Result<&FakeRuntime> {
        let owners = self.owners.lock().unwrap();
        let index = owners
            .get(&session.session_id)
            .ok_or_else(|| Error::NotFound("session".into()))?;
        Ok(&self.runtimes[*index])
    }
}

#[async_trait]
impl HostLink for LocalHost {
    async fn prepare_worktree(
        &self,
        _: HostId,
        _: sun_core::host_protocol::Worktree,
    ) -> Result<PathBuf> {
        Ok(std::env::temp_dir())
    }

    async fn start_session(&self, _: HostId, _: &str, spec: SessionSpec) -> Result<SessionHandle> {
        let index = {
            let mut envelopes = self.envelopes.lock().unwrap();
            envelopes.push(spec.envelope.clone());
            (envelopes.len() - 1).min(self.runtimes.len() - 1)
        };
        let runtime = &self.runtimes[index];
        let handle = runtime.start(spec).await?;
        self.owners.lock().unwrap().insert(handle.session_id, index);
        let mut events = runtime.events(&handle);
        let (sink, session_id) = (self.sink.clone(), handle.session_id);
        tokio::spawn(async move {
            let mut seq = 0;
            while let Some(event) = std::future::poll_fn(|cx| events.as_mut().poll_next(cx)).await {
                seq += 1;
                let _ = sink.send(HostEvent {
                    session_id,
                    seq,
                    event,
                });
            }
        });
        Ok(handle)
    }

    async fn steer(&self, session: &SessionHandle, msg: Steer) -> Result<()> {
        self.runtime(session)?.send(session, msg).await
    }

    async fn stop_session(&self, session: &SessionHandle, how: StopMode) -> Result<()> {
        self.runtime(session)?.stop(session, how).await
    }

    async fn is_alive(&self, _: &SessionHandle) -> Result<bool> {
        Ok(false)
    }

    fn events(&self) -> BoxStream<'static, HostEvent> {
        let source = self.source.lock().unwrap().take().unwrap();
        Box::pin(Receiver(source))
    }

    async fn ack(&self, _: SessionId, _: u64) -> Result<()> {
        Ok(())
    }
}

struct Receiver(mpsc::UnboundedReceiver<HostEvent>);

impl futures_core::Stream for Receiver {
    type Item = HostEvent;
    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<HostEvent>> {
        self.0.poll_recv(cx)
    }
}

struct NoMods;

#[async_trait]
impl ModLink for NoMods {
    async fn call(&self, _: ModCall) -> Result<Value> {
        Err(Error::Unavailable("no mods".into()))
    }
}

/// How a run ended.
struct Outcome {
    status: RunStatus,
    reason: Option<String>,
    /// `(to, reason)` of every step move, in order.
    moves: Vec<(String, Option<String>)>,
    /// Envelope of each session started.
    envelopes: Vec<String>,
    /// Echoes of steers (user text the fake sent back).
    steers: usize,
    /// `decision` event payloads.
    decisions: Vec<Value>,
}

impl Outcome {
    fn moved_to(&self) -> Vec<&str> {
        self.moves.iter().map(|(to, _)| to.as_str()).collect()
    }

    fn reason_of(&self, to: &str) -> &str {
        self.moves
            .iter()
            .find(|(t, _)| t == to)
            .and_then(|(_, why)| why.as_deref())
            .unwrap_or_default()
    }
}

/// Runs the `build` step once per script (session n runs script n).
async fn run(scripts: &[&[&str]], watchdog: WatchdogConfig, budget: Budget) -> Outcome {
    run_workflow(WORKFLOW, scripts, watchdog, budget, None, Spend::default()).await
}

/// Spend caps set before the run starts.
#[derive(Default)]
struct Spend {
    /// The ecosystem's monthly cap.
    monthly_usd: Option<f64>,
    /// The task's own budget.
    task_usd: Option<f64>,
}

/// [`run`] with workflow file `yaml`, a decision backend and spend caps.
async fn run_workflow(
    yaml: &str,
    scripts: &[&[&str]],
    watchdog: WatchdogConfig,
    budget: Budget,
    decisions: Option<DecisionClient>,
    spend: Spend,
) -> Outcome {
    let dir = tempfile::tempdir().unwrap();
    let binary = fake_binary();
    let runtimes = scripts
        .iter()
        .enumerate()
        .map(|(i, lines)| {
            let path = dir.path().join(format!("script-{i}.jsonl"));
            std::fs::write(&path, lines.join("\n")).unwrap();
            FakeRuntime::new(format!("fake-{i}"), &binary, path)
        })
        .collect();
    let (sink, source) = mpsc::unbounded_channel();
    let host = Arc::new(LocalHost {
        runtimes,
        owners: Mutex::new(HashMap::new()),
        envelopes: Mutex::new(Vec::new()),
        sink,
        source: Mutex::new(Some(source)),
    });

    let store = Store::open(dir.path()).await.unwrap();
    let parsed = sun_workflow::parse(yaml).unwrap();
    let workflow = Workflow {
        id: WorkflowId::generate(),
        workspace_id: sun_core::WorkspaceId::MAIN,
        name: parsed.name.clone(),
    };
    let task = Task {
        title: "watched".into(),
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
        budget,
        repo_ids: Vec::new(),
    };
    let mut tx = store.begin().await.unwrap();
    tx.insert_workflow(&workflow).await.unwrap();
    tx.save_workflow_version(workflow.id, yaml, &parsed.hash)
        .await
        .unwrap();
    tx.insert_task(&task).await.unwrap();
    tx.insert_run(&run).await.unwrap();
    tx.commit().await.unwrap();

    let worktrees = dir.path().join("worktrees").join(run.id.to_string());
    std::fs::create_dir_all(&worktrees).unwrap();
    let (engine, _task) = Engine::start_with(
        store.clone(),
        host.clone(),
        Arc::new(NoMods),
        EngineConfig {
            host: HostId::generate(),
            adapter: "fake".into(),
            delivery_timeout: Duration::from_secs(5),
            watchdog,
        },
        decisions,
        None,
    )
    .await
    .unwrap();
    engine.set_spend_cap(spend.monthly_usd).await.unwrap();
    engine
        .set_task_budget(task.id, spend.task_usd)
        .await
        .unwrap();
    engine.start_run(run.id).await.unwrap();

    let deadline = Instant::now() + Duration::from_secs(20);
    let ended = loop {
        let row = store.read().await.unwrap().get_run(run.id).await.unwrap();
        if row.status.is_terminal() {
            break row;
        }
        assert!(Instant::now() < deadline, "run still {}", row.status);
        tokio::time::sleep(Duration::from_millis(20)).await;
    };

    let events = store
        .read()
        .await
        .unwrap()
        .events_after(Some(run.id), 0, 10_000)
        .await
        .unwrap();
    let moves = events
        .iter()
        .filter(|e| e.kind == "step_status")
        .map(|e| {
            let to = e.payload["to"].as_str().unwrap().to_owned();
            (to, e.payload["reason"].as_str().map(str::to_owned))
        })
        .collect();
    let steers = events
        .iter()
        .filter(|e| e.kind == "text" && e.payload["role"] == "user")
        .count();
    let envelopes = host.envelopes.lock().unwrap().clone();
    let decisions = events
        .iter()
        .filter(|e| e.kind == "decision")
        .map(|e| e.payload.clone())
        .collect();
    Outcome {
        decisions,
        status: ended.status,
        reason: ended.end_reason,
        moves,
        envelopes,
        steers,
    }
}

/// For the stall tests: short, but well above a loaded machine's process
/// start, so only the scripted `HANG` stalls.
fn fast(on_stall: StallPolicy) -> WatchdogConfig {
    WatchdogConfig {
        stall_after: Duration::from_millis(1500),
        on_stall,
        ..WatchdogConfig::default()
    }
}

/// For every other test: a stall can't happen, so slow starts under
/// parallel load never turn into a stall.
fn calm() -> WatchdogConfig {
    WatchdogConfig {
        stall_after: Duration::from_secs(60),
        on_stall: StallPolicy::Fail,
        ..WatchdogConfig::default()
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn watchdog_lets_a_working_agent_finish() {
    let out = run(&[&[TALK, BASH, DONE, EXIT]], calm(), Budget::default()).await;
    assert_eq!(out.status, RunStatus::Succeeded, "{:?}", out.reason);
}

#[tokio::test(flavor = "multi_thread")]
async fn watchdog_nudges_once_then_fails_a_stall() {
    let out = run(
        &[&[TALK, HANG]],
        fast(StallPolicy::Nudge),
        Budget::default(),
    )
    .await;
    assert_eq!(out.status, RunStatus::Stalled);
    assert_eq!(out.reason.as_deref(), Some("step `build` stalled"));
    assert_eq!(
        out.moved_to(),
        [
            "queued",
            "dispatched",
            "running",
            "stalled",
            "running",
            "stalled"
        ]
    );
    assert_eq!(out.reason_of("stalled"), "no agent activity for 1.5s");
    assert_eq!(out.steers, 1, "the nudge reached the agent once");
}

/// The workflow's `watchdog:` beats the engine-wide default (15 min, nudge).
#[tokio::test(flavor = "multi_thread")]
async fn stall_after_from_workflow_ends_stalled() {
    let yaml = format!("{WORKFLOW}watchdog: {{ stall_after: 2s, on_stall: fail }}\n");
    let out = run_workflow(
        &yaml,
        &[&[TALK, HANG]],
        WatchdogConfig::default(),
        Budget::default(),
        None,
        Spend::default(),
    )
    .await;
    assert_eq!(out.status, RunStatus::Stalled);
    assert_eq!(
        out.moved_to(),
        ["queued", "dispatched", "running", "stalled"]
    );
    assert_eq!(out.reason_of("stalled"), "no agent activity for 2s");
    assert_eq!(out.steers, 0, "fail: no nudge");
}

#[tokio::test(flavor = "multi_thread")]
async fn watchdog_journals_a_decision_with_a_fake_backend() {
    // The rule says stuck (stall policy: fail); the model says looping.
    let backend = support::systemone(
        r#"{"answers": {"decision": {"choice": "looping", "answer_confidence": 0.9}},
            "routing": {"model": "laya-1"}}"#,
    );
    let out = run_workflow(
        WORKFLOW,
        &[&[TALK, BASH, HANG]],
        fast(StallPolicy::Fail),
        Budget::default(),
        Some(backend),
        Spend::default(),
    )
    .await;
    assert_eq!(
        out.moved_to(),
        ["queued", "dispatched", "running", "looping"]
    );
    assert!(out.reason_of("looping").contains("decision engine"));
    let decision = &out.decisions[0];
    assert_eq!(decision["decides"], "watchdog");
    assert_eq!(decision["acted"], true);
    assert_eq!(decision["answer"], serde_json::json!({"choice": "looping"}));
    assert_eq!(decision["rule"], serde_json::json!({"choice": "stuck"}));
    assert_eq!(decision["model"], "laya-1");
    assert_eq!(decision["confidence"], 0.9);
    let recent = decision["state"]["recent"].to_string();
    assert!(recent.contains("tool Bash: cargo test"), "{recent}");
}

#[tokio::test(flavor = "multi_thread")]
async fn watchdog_restarts_a_stall_with_a_hand_off() {
    let out = run(
        &[&[TALK, HANG], &[TALK, DONE, EXIT]],
        fast(StallPolicy::Restart),
        Budget::default(),
    )
    .await;
    assert_eq!(out.status, RunStatus::Succeeded, "{:?}", out.reason);
    assert_eq!(
        out.moved_to(),
        [
            "queued",
            "dispatched",
            "running",
            "stalled",
            "handing_off",
            "dispatched",
            "running",
            "verifying",
            "succeeded"
        ]
    );
    assert_eq!(out.envelopes.len(), 2);
    assert!(out.envelopes[1].contains("build it\n\nHand-off from Sun"));
}

#[tokio::test(flavor = "multi_thread")]
async fn watchdog_stops_a_loop() {
    let out = run(
        &[&[TALK, BASH, BASH, BASH, HANG]],
        calm(),
        Budget::default(),
    )
    .await;
    assert_eq!(out.status, RunStatus::Looping);
    assert_eq!(
        out.reason_of("looping"),
        "`Bash` called 3 times with the same arguments"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn watchdog_enforces_cost_and_wall_budgets() {
    let cost = Budget {
        cost_usd: Some(1.0),
        ..Budget::default()
    };
    let out = run(&[&[TALK, SPEND, SPEND, HANG]], calm(), cost).await;
    assert_eq!(out.status, RunStatus::BudgetExceeded);
    assert_eq!(
        out.reason_of("budget_exceeded"),
        "cost $1.20 exceeds the $1.00 budget"
    );

    // Activity keeps the stall timer quiet; the wall clock still runs out.
    let wall = Budget {
        wall_secs: Some(1),
        ..Budget::default()
    };
    let busy: Vec<String> = (0..40)
        .map(|i| {
            format!(
                r#"{{"delay_ms":100,"kind":"tool_call","name":"Read","args_hash":{i},"summary":"read"}}"#
            )
        })
        .collect();
    let busy: Vec<&str> = busy.iter().map(String::as_str).collect();
    let out = run(&[&busy], calm(), wall).await;
    assert_eq!(out.status, RunStatus::BudgetExceeded);
    assert_eq!(
        out.reason_of("budget_exceeded"),
        "wall time budget of 1s exceeded"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn watchdog_fails_empty_output_and_silent_exits() {
    let out = run(&[&[DONE, EXIT]], calm(), Budget::default()).await;
    assert_eq!(out.status, RunStatus::Failed);
    assert!(out.reason_of("failed").starts_with("empty output"));

    let out = run(
        &[&[TALK, r#"{"kind":"exited","code":3}"#]],
        calm(),
        Budget::default(),
    )
    .await;
    assert_eq!(out.status, RunStatus::Failed);
    assert_eq!(out.reason.as_deref(), Some("step `build` failed"));
    // The driver turns an exit without done into an `Error` first, then `Exited`.
    assert_eq!(
        out.reason_of("failed"),
        "the agent exited (code 3) without claiming done"
    );
}

/// The task's own budget is below the workflow's, so it ends the run.
#[tokio::test(flavor = "multi_thread")]
async fn task_budget_beats_workflow_budget() {
    let workflow = Budget {
        cost_usd: Some(5.0),
        ..Budget::default()
    };
    let spend = Spend {
        task_usd: Some(1.0),
        ..Spend::default()
    };
    let script: &[&str] = &[TALK, SPEND, SPEND, HANG];
    let out = run_workflow(WORKFLOW, &[script], calm(), workflow, None, spend).await;
    assert_eq!(out.status, RunStatus::BudgetExceeded);
    assert_eq!(
        out.reason_of("budget_exceeded"),
        "cost $1.20 exceeds the $1.00 budget"
    );

    // A looser task budget leaves the workflow's in charge.
    let workflow = Budget {
        cost_usd: Some(1.0),
        ..Budget::default()
    };
    let spend = Spend {
        task_usd: Some(5.0),
        ..Spend::default()
    };
    let out = run_workflow(WORKFLOW, &[script], calm(), workflow, None, spend).await;
    assert_eq!(
        out.reason_of("budget_exceeded"),
        "cost $1.20 exceeds the $1.00 budget"
    );
}

/// A running step ends once the ecosystem's month reaches its cap,
/// and with the cap already reached an agent step doesn't start.
#[tokio::test(flavor = "multi_thread")]
async fn ecosystem_spend_cap_ends_a_running_step() {
    let spend = Spend {
        monthly_usd: Some(1.0),
        ..Spend::default()
    };
    let script: &[&str] = &[TALK, SPEND, SPEND, HANG];
    let out = run_workflow(WORKFLOW, &[script], calm(), Budget::default(), None, spend).await;
    assert_eq!(out.status, RunStatus::BudgetExceeded);
    assert_eq!(
        out.reason_of("budget_exceeded"),
        "Ecosystem spend $1.20 reached the $1.00 monthly cap"
    );

    let spend = Spend {
        monthly_usd: Some(0.0),
        ..Spend::default()
    };
    let out = run_workflow(WORKFLOW, &[script], calm(), Budget::default(), None, spend).await;
    assert_eq!(out.status, RunStatus::BudgetExceeded);
    assert_eq!(out.moved_to(), ["queued", "running", "budget_exceeded"]);
    assert!(out.envelopes.is_empty(), "no session started");
}
