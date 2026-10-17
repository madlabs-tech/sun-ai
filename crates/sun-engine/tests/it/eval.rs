//! Eval replay end to end: a task fails its test gate on the
//! scripted fake runtime; replaying its recorded transcript reproduces the
//! failure; replaying it with a candidate memory line on the fake runtime
//! (whose script follows the line when the pack carries it) passes, and the
//! `open_pr` after approval is recorded, never delivered.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::process::Command as Process;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use serde_json::{Value, json};
use sun_core::host_protocol::{ExecOutput, ExecRequest, SessionFile, Worktree};
use sun_core::model::{Budget, CorrectionCause, EvalRun, EvalStatus, Repo, Run, Task, Workflow};
use sun_core::{
    BoxStream, Error, HostEvent, HostId, HostLink, ModCall, ModLink, Result, RunId, RunStatus,
    RuntimeAdapter, SessionHandle, SessionId, SessionSpec, StateMachine, Steer, StopMode, TaskId,
    TaskStatus, WorkflowId,
};
use sun_engine::{Candidate, Engine, EngineConfig, EvalReport, EvalSpec, Pick, WatchdogConfig};
use sun_runtime::FakeRuntime;
use sun_store::{EventLog, RepoRepo, RunRepo, Store, TaskRepo, WorkflowRepo};
use tokio::sync::mpsc;

const LINE: &str = "Write exactly ok to work.txt.";

const WORKFLOW: &str = r#"
name: evals
steps:
  - id: implement
    type: agent
    prompt: fill work.txt
  - id: verify
    type: gate
    gates: [{test: {command: "grep -qx ok work.txt"}}]
    on_fail: {send_back_to: implement, max: 1}
  - id: review
    type: approval
    who: [user:lead]
  - id: pr
    type: action
    use: github.open_pr
"#;

fn script(text: &str) -> String {
    [
        r#"{"kind":"text","role":"assistant","text":"on it"}"#.to_owned(),
        format!(r#"{{"kind":"file_changed","path":"work.txt","write":"{text}\n"}}"#),
        r#"{"kind":"done_claimed","summary":"filled work.txt"}"#.to_owned(),
        r#"{"kind":"exited","code":0}"#.to_owned(),
    ]
    .join("\n")
}

fn fake_binary() -> PathBuf {
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
    let status = Process::new(cargo)
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
    let exe = std::env::current_exe().unwrap();
    exe.parent()
        .unwrap()
        .parent()
        .unwrap()
        .join("sun-fake-runtime")
}

fn sh(cwd: &Path, command: &str) -> (Option<i32>, String) {
    let out = Process::new("sh")
        .arg("-c")
        .arg(command)
        .current_dir(cwd)
        .output()
        .unwrap();
    let text =
        String::from_utf8_lossy(&out.stdout).into_owned() + &String::from_utf8_lossy(&out.stderr);
    (out.status.code(), text)
}

/// A host with one worktree per task. Its `fake` sessions follow [`LINE`]
/// when the context pack carries it, else write `wip`.
struct LocalHost {
    work: PathBuf,
    /// `[ignores the line, follows it]`.
    runtimes: [FakeRuntime; 2],
    sinks: Mutex<Vec<mpsc::UnboundedSender<HostEvent>>>,
    sessions: Mutex<Vec<(SessionId, usize)>>,
    /// Tasks whose worktree was removed.
    removed: Mutex<Vec<TaskId>>,
}

impl LocalHost {
    fn runtime(&self, id: SessionId) -> Result<&FakeRuntime> {
        let sessions = self.sessions.lock().unwrap();
        let (_, index) = sessions
            .iter()
            .find(|(s, _)| *s == id)
            .ok_or_else(|| Error::NotFound("session".into()))?;
        Ok(&self.runtimes[*index])
    }
}

#[async_trait]
impl HostLink for LocalHost {
    async fn start_session(
        &self,
        _: HostId,
        adapter: &str,
        spec: SessionSpec,
    ) -> Result<SessionHandle> {
        assert_eq!(adapter, "fake", "only the fake runtime runs for real here");
        let pack = std::fs::read_to_string(spec.worktree.join(sun_context::PACK_PATH)).unwrap();
        let index = usize::from(pack.contains(LINE));
        let runtime = &self.runtimes[index];
        let handle = runtime.start(spec).await?;
        self.sessions
            .lock()
            .unwrap()
            .push((handle.session_id, index));
        let mut events = runtime.events(&handle);
        let sinks = self.sinks.lock().unwrap().clone();
        let session_id = handle.session_id;
        tokio::spawn(async move {
            let mut seq = 0;
            while let Some(event) = std::future::poll_fn(|cx| events.as_mut().poll_next(cx)).await {
                seq += 1;
                for sink in &sinks {
                    let _ = sink.send(HostEvent {
                        session_id,
                        seq,
                        event: event.clone(),
                    });
                }
            }
        });
        Ok(handle)
    }

    async fn steer(&self, session: &SessionHandle, msg: Steer) -> Result<()> {
        self.runtime(session.session_id)?.send(session, msg).await
    }

    async fn stop_session(&self, session: &SessionHandle, how: StopMode) -> Result<()> {
        self.runtime(session.session_id)?.stop(session, how).await
    }

    async fn is_alive(&self, _: &SessionHandle) -> Result<bool> {
        Ok(false)
    }

    fn events(&self) -> BoxStream<'static, HostEvent> {
        let (sink, source) = mpsc::unbounded_channel();
        self.sinks.lock().unwrap().push(sink);
        Box::pin(Receiver(source))
    }

    async fn ack(&self, _: SessionId, _: u64) -> Result<()> {
        Ok(())
    }

    async fn prepare_worktree(&self, _: HostId, worktree: Worktree) -> Result<PathBuf> {
        let dir = self.work.join(worktree.task_id().to_string());
        if dir.exists() {
            return Ok(dir);
        }
        std::fs::create_dir_all(&dir).unwrap();
        let init = match &worktree {
            // Like the real host: a branch cut from the remote base's tip.
            Worktree::Repo { url, branch, .. } => {
                format!("git clone -q {url} . && git checkout -q -b {branch}")
            }
            _ => "git init -q -b main && git -c user.email=t@t -c user.name=t \
                  commit -q --allow-empty -m base"
                .into(),
        };
        let (code, out) = sh(&dir, &init);
        assert_eq!(code, Some(0), "{out}");
        Ok(dir)
    }

    async fn remove_worktree(&self, _: HostId, task_id: TaskId) -> Result<()> {
        let _ = std::fs::remove_dir_all(self.work.join(task_id.to_string()));
        self.removed.lock().unwrap().push(task_id);
        Ok(())
    }

    async fn write_files(&self, _: HostId, cwd: PathBuf, files: Vec<SessionFile>) -> Result<()> {
        sun_context::write_files(&cwd, &files).map_err(|e| Error::Unavailable(e.to_string()))
    }

    async fn exec(&self, _: HostId, request: ExecRequest) -> Result<ExecOutput> {
        let (code, output) = sh(&request.cwd, &request.command);
        Ok(ExecOutput {
            code,
            output,
            timed_out: false,
        })
    }
}

struct Receiver(mpsc::UnboundedReceiver<HostEvent>);

impl futures_core::Stream for Receiver {
    type Item = HostEvent;
    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<HostEvent>> {
        self.0.poll_recv(cx)
    }
}

/// The real mods: any call here is a delivered side effect.
#[derive(Default)]
struct Mods(Mutex<Vec<ModCall>>);

#[async_trait]
impl ModLink for Mods {
    async fn call(&self, call: ModCall) -> Result<Value> {
        self.0.lock().unwrap().push(call);
        Ok(json!({}))
    }
}

async fn until<T>(what: &str, mut check: impl AsyncFnMut() -> Option<T>) -> T {
    let deadline = Instant::now() + Duration::from_secs(120);
    loop {
        if let Some(done) = check().await {
            return done;
        }
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

async fn finished(engine: &Engine, eval: EvalRun) -> EvalReport {
    let done = until("the eval", async || {
        let run = engine.eval_run(eval.id).await.unwrap();
        (run.status != EvalStatus::Running).then_some(run)
    })
    .await;
    assert_eq!(done.status, EvalStatus::Done, "{:?}", done.error);
    serde_json::from_value(done.report.unwrap()).unwrap()
}

/// A [`LocalHost`] in `dir` whose fake agents write `wip`, or `ok` when
/// the pack carries [`LINE`].
fn local_host(dir: &Path) -> Arc<LocalHost> {
    let binary = fake_binary();
    let runtimes = ["wip", "ok"].map(|text| {
        let path = dir.join(format!("{text}.jsonl"));
        std::fs::write(&path, script(text)).unwrap();
        FakeRuntime::new("fake", &binary, path)
    });
    Arc::new(LocalHost {
        work: dir.join("work"),
        runtimes,
        sinks: Mutex::default(),
        sessions: Mutex::default(),
        removed: Mutex::default(),
    })
}

/// A store holding `yaml`'s workflow, one task on `repos` and its queued
/// run, and an engine on it with the fake runtime.
async fn seed(
    dir: &Path,
    host: Arc<LocalHost>,
    mods: Arc<Mods>,
    yaml: &str,
    repos: Vec<Repo>,
) -> (Store, Engine, Task, Run) {
    let store = Store::open(dir.join("data")).await.unwrap();
    let parsed = sun_workflow::parse(yaml).unwrap();
    let workflow = Workflow {
        id: WorkflowId::generate(),
        workspace_id: sun_core::WorkspaceId::MAIN,
        name: parsed.name.clone(),
    };
    let task = Task {
        title: "fill work.txt".into(),
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
        repo_ids: repos.iter().map(|r| r.id).collect(),
    };
    let mut tx = store.begin().await.unwrap();
    tx.insert_workflow(&workflow).await.unwrap();
    tx.save_workflow_version(workflow.id, yaml, &parsed.hash)
        .await
        .unwrap();
    for repo in &repos {
        tx.insert_repo(repo).await.unwrap();
    }
    tx.insert_task(&task).await.unwrap();
    tx.insert_run(&run).await.unwrap();
    tx.commit().await.unwrap();
    let config = EngineConfig {
        allow_bypass: false,
        host: HostId::generate(),
        adapter: "fake".into(),
        delivery_timeout: Duration::from_secs(5),
        watchdog: WatchdogConfig::default(),
    };
    let (engine, _task) = Engine::start(store.clone(), host, mods, config)
        .await
        .unwrap();
    (store, engine, task, run)
}

/// Starts `run` and waits for its end.
async fn run_to_end(store: &Store, engine: &Engine, run: &Run) -> Run {
    engine.start_run(run.id).await.unwrap();
    until("the original run", async || {
        let run = store.read().await.unwrap().get_run(run.id).await.unwrap();
        run.status.is_terminal().then_some(run)
    })
    .await
}

#[tokio::test(flavor = "multi_thread")]
async fn eval_replays_past_tasks_and_reports_the_delta() {
    let dir = tempfile::tempdir().unwrap();
    let host = local_host(dir.path());
    let mods = Arc::new(Mods::default());
    // The past task: its agent ignores the rule twice and the run fails.
    let (store, engine, task, run) =
        seed(dir.path(), host.clone(), mods.clone(), WORKFLOW, vec![]).await;
    let ended = run_to_end(&store, &engine, &run).await;
    assert_eq!(ended.status, RunStatus::Failed);

    // Its recorded transcript is a fake-runtime script.
    let transcript = engine.transcript(run.id).await.unwrap();
    let implement = &transcript[&sun_core::StepKey::new("implement").unwrap()];
    assert!(
        implement.contains("file_changed") && implement.ends_with(r#"{"kind":"exited","code":0}"#)
    );

    // 1. The recorded transcripts, no candidate: the same failure again.
    let spec = EvalSpec {
        name: "baseline".into(),
        pick: Pick::Cause {
            cause: CorrectionCause::TestFailure,
        },
        candidate: Candidate::default(),
        runtime: None,
        limit: None,
        auto_approve: false,
    };
    let report = finished(&engine, engine.start_eval(spec).await.unwrap()).await;
    assert_eq!(report.runtime, "replay");
    assert_eq!(report.replayed, 1);
    let result = &report.tasks[0];
    assert_eq!(result.task_id, task.id);
    assert_eq!(result.original.causes, [CorrectionCause::TestFailure; 2]);
    assert_eq!(result.original.repeats, 1);
    let replay = result.replay.as_ref().unwrap();
    assert_eq!(replay.status, RunStatus::Failed);
    assert_eq!(replay.causes, result.original.causes);
    assert_eq!((report.pass_delta, report.repeat_delta), (0.0, 0));

    // 2. A candidate memory line on the fake runtime, from a saved set.
    let set = engine
        .save_eval_set("work.txt tasks", vec![task.id])
        .await
        .unwrap();
    let spec = EvalSpec {
        name: "rule for work.txt".into(),
        pick: Pick::Set { set_id: set.id },
        candidate: Candidate {
            workflow: None,
            memory: vec![LINE.into()],
        },
        runtime: Some("fake".into()),
        limit: Some(5),
        auto_approve: false,
    };
    let report = finished(&engine, engine.start_eval(spec).await.unwrap()).await;
    let replay = report.tasks[0].replay.as_ref().unwrap();
    assert_eq!(
        replay.status,
        RunStatus::Succeeded,
        "{:?}",
        replay.end_reason
    );
    assert!(replay.causes.is_empty());
    assert_eq!(
        replay.actions,
        ["github.open_pr"],
        "recorded, not delivered"
    );
    assert_eq!(
        (report.original.pass_rate, report.replay.pass_rate),
        (0.0, 1.0)
    );
    assert_eq!((report.pass_delta, report.repeat_delta), (1.0, -1));

    // Nothing reached the real mods, the original task is untouched, and each
    // replay's own worktree (a copy's) was removed.
    assert!(mods.0.lock().unwrap().is_empty());
    let copies: std::collections::HashSet<TaskId> = host
        .removed
        .lock()
        .unwrap()
        .iter()
        .copied()
        .filter(|id| *id != task.id)
        .collect();
    assert_eq!(copies.len(), 2);
    let mut read = store.read().await.unwrap();
    assert_eq!(read.runs_of_task(task.id).await.unwrap().len(), 1);
    assert_eq!(
        read.get_task(task.id).await.unwrap().status,
        TaskStatus::Failed
    );
    assert_eq!(engine.eval_runs(10).await.unwrap().len(), 2);
}

const BASED: &str = r#"
name: based
steps:
  - id: implement
    type: agent
    prompt: fill work.txt
  - id: verify
    type: gate
    gates: [{test: {command: "test ! -f later.txt"}}]
"#;

/// A replay cuts its worktree from the commit the original run started
/// from, so work merged into the base later never leaks into it.
#[tokio::test(flavor = "multi_thread")]
async fn eval_replays_from_the_original_base_commit() {
    let dir = tempfile::tempdir().unwrap();
    let origin = dir.path().join("origin");
    std::fs::create_dir_all(&origin).unwrap();
    let git = "git -c user.email=t@t -c user.name=t";
    let (code, out) = sh(
        &origin,
        &format!("git init -q -b main && {git} commit -q --allow-empty -m base"),
    );
    assert_eq!(code, Some(0), "{out}");
    let repo = Repo {
        id: sun_core::RepoId::generate(),
        workspace_id: sun_core::WorkspaceId::MAIN,
        url: origin.display().to_string(),
        default_branch: "main".into(),
        gate_config: json!({}),
        description: String::new(),
        hook_id: None,
    };
    let host = local_host(dir.path());
    let (store, engine, task, run) = seed(
        dir.path(),
        host.clone(),
        Arc::new(Mods::default()),
        BASED,
        vec![repo],
    )
    .await;
    let ended = run_to_end(&store, &engine, &run).await;
    assert_eq!(ended.status, RunStatus::Succeeded, "{:?}", ended.end_reason);
    let base = sh(&origin, "git rev-parse HEAD").1.trim().to_owned();
    let journaled = store
        .read()
        .await
        .unwrap()
        .events_after(Some(run.id), 0, 500)
        .await
        .unwrap()
        .into_iter()
        .find(|e| e.kind == "run_base")
        .expect("run_base journaled");
    assert_eq!(
        journaled.payload["repos"],
        json!([{ "url": origin.display().to_string(), "sha": base }])
    );

    // Later work lands on the base branch; the replay must not see it.
    let (code, out) = sh(
        &origin,
        &format!("touch later.txt && git add later.txt && {git} commit -q -m later"),
    );
    assert_eq!(code, Some(0), "{out}");
    let spec = EvalSpec {
        name: "base".into(),
        pick: Pick::Tasks {
            task_ids: vec![task.id],
        },
        candidate: Candidate::default(),
        runtime: None,
        limit: None,
        auto_approve: false,
    };
    let report = finished(&engine, engine.start_eval(spec).await.unwrap()).await;
    let result = &report.tasks[0];
    let replay = result.replay.as_ref().expect("replayed");
    assert_eq!(
        replay.status,
        RunStatus::Succeeded,
        "{:?} {:?}",
        replay.end_reason,
        result.error
    );
    assert!(replay.causes.is_empty());
}
