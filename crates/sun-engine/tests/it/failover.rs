//! Failover end to end: an agent step on the scripted fake runtime hits
//! its limit mid-step; the engine checkpoints the worktree, hands off to the
//! next runtime of the chain in the same worktree, and the gate step re-checks
//! the work. The step attempt never re-runs.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use crate::support;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::process::Command as Process;
use std::sync::{Arc, Mutex, OnceLock};
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use serde_json::{Value, json};
use sun_core::host_protocol::{ExecOutput, ExecRequest, SessionFile, Worktree};
use sun_core::model::{Budget, EventRecord, Host, OutboxStatus, Repo, Run, Task, Workflow};
use sun_core::{
    BoxStream, EngineHandle, Error, HostEvent, HostId, HostLink, ModCall, ModLink, RepoId, Result,
    RunId, RunStatus, RuntimeAdapter, SessionHandle, SessionId, SessionSpec, StateMachine, Steer,
    StepKey, StopMode, TaskId, TaskStatus, WorkflowId,
};
use sun_engine::{DecisionClient, Engine, EngineConfig, MarketPolicy, Principal, WatchdogConfig};
use sun_runtime::FakeRuntime;
use sun_store::{
    EventLog, HostRepo, OutboxRepo, RepoRepo, RunRepo, SettingRepo, Store, TaskContext, TaskRepo,
    WorkflowRepo, WorkspaceRepo,
};
use sun_workflow::OnLimit;
use tokio::sync::mpsc;

const TALK: &str = r#"{"kind":"text","role":"assistant","text":"on it"}"#;
const EDIT: &str = r#"{"kind":"file_changed","path":"work.txt"}"#;
const DONE: &str = r#"{"kind":"done_claimed","summary":"finished"}"#;
const EXIT: &str = r#"{"kind":"exited","code":0}"#;

/// A limit that resets `reset_in` after the fake emits it (not after the
/// script is written: setting up a harness under load can take longer).
fn limit_hit(reset_in: Duration) -> String {
    format!(
        r#"{{"kind":"limit_hit","limit":"quota","reset_at":null,"reset_in_ms":{},"raw":"usage limit reached"}}"#,
        reset_in.as_millis()
    )
}

/// `runtime_extra`: more `runtime:` keys; `pr`: a final `github.open_pr` step.
fn workflow(on_limit: &str, runtime_extra: &str, pr: bool, chain: &str) -> String {
    let pr = if pr {
        "  - {id: pr, type: action, use: github.open_pr}\n"
    } else {
        ""
    };
    let chain = if chain.is_empty() {
        "fake-limit, fake-ok"
    } else {
        chain
    };
    format!(
        r#"
name: failover
runtime:
  chain: [{chain}]
  on_limit: {on_limit}
  {runtime_extra}
steps:
  - id: implement
    type: agent
    prompt: implement it
  - id: verify
    type: gate
    gates: [diff_not_empty, {{test: {{command: "git ls-files --error-unmatch work.txt"}}}}]
{pr}"#
    )
}

/// Builds `sun-fake-runtime` once (a no-op when it is fresh) and returns its path.
fn fake_binary() -> PathBuf {
    static BINARY: OnceLock<PathBuf> = OnceLock::new();
    BINARY
        .get_or_init(|| {
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
        })
        .clone()
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

/// An in-process host with one worktree. Each adapter id has a queue of
/// fake runtimes: its n-th session runs the n-th one (the last repeats).
/// With `seller`, a second host (a marketplace seller's) whose worktree is
/// a fresh clone of `../origin.git` on the task's branch, as `sun-host`'s
/// prepare checks out a pushed `sun/<task_id>`.
struct LocalHost {
    worktree: PathBuf,
    seller: Option<(HostId, PathBuf)>,
    /// The host of every session started.
    started_on: Mutex<Vec<HostId>>,
    runtimes: HashMap<String, Vec<FakeRuntime>>,
    /// `(adapter, envelope)` of every session started.
    starts: Mutex<Vec<(String, String)>>,
    /// `spec.resume` of every session started.
    resumes: Mutex<Vec<Option<String>>>,
    owners: Mutex<HashMap<SessionId, (String, usize)>>,
    /// The current `events()` subscriber (a new one per engine start).
    sink: Mutex<mpsc::UnboundedSender<HostEvent>>,
    /// This many more `start_session` calls fail as if the host were offline.
    offline: Mutex<usize>,
    /// Answers of the next probes; then `false` (sessions don't survive).
    probes: Mutex<Vec<Result<bool>>>,
}

impl LocalHost {
    fn runtime(&self, session: &SessionHandle) -> Result<&FakeRuntime> {
        let owners = self.owners.lock().unwrap();
        let (adapter, index) = owners
            .get(&session.session_id)
            .ok_or_else(|| Error::NotFound("session".into()))?;
        Ok(&self.runtimes[adapter][*index])
    }

    /// `host`'s task worktree.
    fn dir(&self, host: HostId) -> &PathBuf {
        match &self.seller {
            Some((seller, dir)) if *seller == host => dir,
            _ => &self.worktree,
        }
    }
}

#[async_trait]
impl HostLink for LocalHost {
    async fn start_session(
        &self,
        host: HostId,
        adapter: &str,
        spec: SessionSpec,
    ) -> Result<SessionHandle> {
        assert_eq!(
            &spec.worktree,
            self.dir(host),
            "same worktree for every session of a host"
        );
        self.started_on.lock().unwrap().push(host);
        assert!(
            spec.worktree.join(sun_context::PACK_PATH).exists(),
            "context files are written before the session starts"
        );
        {
            let mut offline = self.offline.lock().unwrap();
            if *offline > 0 {
                *offline -= 1;
                return Err(Error::Unavailable("host is offline".into()));
            }
        }
        self.resumes.lock().unwrap().push(spec.resume.clone());
        let queue = self
            .runtimes
            .get(adapter)
            .ok_or_else(|| Error::NotFound(adapter.into()))?;
        let index = {
            let mut starts = self.starts.lock().unwrap();
            let seen = starts.iter().filter(|(a, _)| a == adapter).count();
            starts.push((adapter.to_owned(), spec.envelope.clone()));
            seen.min(queue.len() - 1)
        };
        let runtime = &queue[index];
        let handle = runtime.start(spec).await?;
        self.owners
            .lock()
            .unwrap()
            .insert(handle.session_id, (adapter.to_owned(), index));
        let mut events = runtime.events(&handle);
        let (sink, session_id) = (self.sink.lock().unwrap().clone(), handle.session_id);
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
        let mut probes = self.probes.lock().unwrap();
        if probes.is_empty() {
            Ok(false)
        } else {
            probes.remove(0)
        }
    }

    fn events(&self) -> BoxStream<'static, HostEvent> {
        let (sink, source) = mpsc::unbounded_channel();
        *self.sink.lock().unwrap() = sink;
        Box::pin(Receiver(source))
    }

    async fn ack(&self, _: SessionId, _: u64) -> Result<()> {
        Ok(())
    }

    async fn prepare_worktree(&self, host: HostId, worktree: Worktree) -> Result<PathBuf> {
        let dir = self.dir(host);
        if !dir.exists() {
            let clone = format!(
                "git clone -q -b {} origin.git {}",
                worktree.branch().unwrap_or("main"),
                dir.display()
            );
            let (code, out) = sh(self.worktree.parent().unwrap(), &clone);
            assert_eq!(code, Some(0), "{out}");
        }
        Ok(dir.clone())
    }

    async fn write_files(&self, host: HostId, cwd: PathBuf, files: Vec<SessionFile>) -> Result<()> {
        assert_eq!(&cwd, self.dir(host));
        sun_context::write_files(&cwd, &files).map_err(|e| Error::Unavailable(e.to_string()))
    }

    async fn exec(&self, host: HostId, request: ExecRequest) -> Result<ExecOutput> {
        assert_eq!(&request.cwd, self.dir(host));
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

/// No mods; Sun Cloud's marketplace (`sun_cloud.*`) answers and is recorded.
#[derive(Default)]
struct NoMods(Mutex<Vec<ModCall>>);

#[async_trait]
impl ModLink for NoMods {
    async fn call(&self, call: ModCall) -> Result<Value> {
        if call.mod_name != sun_engine::MARKET_MOD {
            return Err(Error::Unavailable("no mods".into()));
        }
        self.0.lock().unwrap().push(call);
        Ok(serde_json::json!({ "escrow": "refunded" }))
    }
}

struct Harness {
    _dir: tempfile::TempDir,
    store: Store,
    engine: Engine,
    host: Arc<LocalHost>,
    mods: Arc<NoMods>,
    run_id: RunId,
    config: EngineConfig,
    task: tokio::task::JoinHandle<Result<()>>,
}

/// Optional host conditions of a [`Harness`].
#[derive(Default)]
struct Setup {
    /// Starts that fail first, as if the host were offline.
    offline: usize,
    /// The host row's `hello` capabilities; `None`: no row (trusted).
    capabilities: Option<Value>,
    /// Context window per adapter id (context rotation).
    context_windows: Vec<(&'static str, u32)>,
    /// The task's pinned constraints.
    pinned: Vec<String>,
    /// More `runtime:` keys of the workflow, e.g. `switch_back: true`.
    runtime_extra: &'static str,
    /// The task has a repo and the workflow ends with `github.open_pr`.
    pr: bool,
    /// `on_limit` set on the task and on the ecosystem.
    on_limit_task: Option<OnLimit>,
    on_limit_ecosystem: Option<OnLimit>,
    /// The decision backend, if any.
    decisions: Option<DecisionClient>,
    /// The workflow's `runtime.chain`; empty: `fake-limit, fake-ok`.
    chain: &'static str,
    /// The ecosystem's marketplace policy, set before the run starts.
    market: Option<MarketPolicy>,
    /// A marketplace seller's host (see [`LocalHost`]).
    seller: bool,
}

impl Harness {
    /// A git worktree with uncommitted work (the first agent's edit) and an
    /// engine on `scripts` per adapter id; the run is started.
    async fn start(on_limit: &str, scripts: &[(&str, Vec<Vec<String>>)]) -> Self {
        Self::with(on_limit, scripts, Setup::default()).await
    }

    async fn with(on_limit: &str, scripts: &[(&str, Vec<Vec<String>>)], setup: Setup) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let binary = fake_binary();
        let mut runtimes = HashMap::new();
        for (adapter, queue) in scripts {
            let list = queue
                .iter()
                .enumerate()
                .map(|(i, lines)| {
                    let path = dir.path().join(format!("{adapter}-{i}.jsonl"));
                    std::fs::write(&path, lines.join("\n")).unwrap();
                    FakeRuntime::new(*adapter, &binary, path)
                })
                .collect();
            runtimes.insert((*adapter).to_owned(), list);
        }

        let store = Store::open(dir.path()).await.unwrap();
        let source = workflow(on_limit, setup.runtime_extra, setup.pr, setup.chain);
        let repo = Repo {
            id: RepoId::generate(),
            workspace_id: sun_core::WorkspaceId::MAIN,
            url: "git@github.com:acme/web.git".into(),
            default_branch: "main".into(),
            gate_config: serde_json::json!({}),
            description: String::new(),
            hook_id: None,
        };
        let parsed = sun_workflow::parse(&source).unwrap();
        let workflow = Workflow {
            id: WorkflowId::generate(),
            workspace_id: sun_core::WorkspaceId::MAIN,
            name: parsed.name.clone(),
        };
        let task = Task {
            title: "failover".into(),
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
            repo_ids: if setup.pr { vec![repo.id] } else { vec![] },
        };
        let mut tx = store.begin().await.unwrap();
        tx.insert_workflow(&workflow).await.unwrap();
        if setup.pr {
            tx.insert_repo(&repo).await.unwrap();
        }
        tx.save_workflow_version(workflow.id, &source, &parsed.hash)
            .await
            .unwrap();
        tx.insert_task(&task).await.unwrap();
        tx.insert_run(&run).await.unwrap();
        // The task's branch, as the engine names it (`sun/<KEY>-<slug>`).
        let branch = sun_core::model::TaskKey {
            prefix: tx
                .get_workspace(sun_core::WorkspaceId::MAIN)
                .await
                .unwrap()
                .issue_prefix,
            number: tx.get_task(task.id).await.unwrap().number,
        }
        .branch(&task.title);
        let pinned = TaskContext {
            pinned: setup.pinned,
            ..TaskContext::default()
        };
        tx.put_task_context(task.id, &pinned).await.unwrap();
        tx.commit().await.unwrap();

        let worktree = dir.path().join("work").join(run.id.to_string());
        std::fs::create_dir_all(&worktree).unwrap();
        let git = "git init -q -b sun/task && git -c user.name=t -c user.email=t@t \
                   commit -q --allow-empty -m base && echo half-done > work.txt";
        assert_eq!(sh(&worktree, git).0, Some(0));
        if setup.pr {
            // The PR step pushes the task branch to `origin` first.
            let origin = format!(
                "git branch -q -m {branch} && git init -q --bare ../origin.git && \
                 git remote add origin ../origin.git"
            );
            assert_eq!(sh(&worktree, &origin).0, Some(0));
        }

        let config = EngineConfig {
            host: HostId::generate(),
            adapter: "unused".into(),
            delivery_timeout: Duration::from_secs(5),
            watchdog: WatchdogConfig {
                dispatch_backoff: Duration::from_millis(20),
                dispatch_tries: 3,
                context_windows: setup
                    .context_windows
                    .iter()
                    .map(|(a, w)| ((*a).to_owned(), *w))
                    .collect(),
                ..WatchdogConfig::default()
            },
        };
        if let Some(capabilities) = setup.capabilities {
            let mut tx = store.begin().await.unwrap();
            let host = Host {
                id: config.host,
                name: "mac-mini".into(),
                capabilities,
            };
            tx.insert_host(&host).await.unwrap();
            tx.commit().await.unwrap();
        }
        let seller = setup
            .seller
            .then(|| (HostId::generate(), dir.path().join("work/seller")));
        let host = Arc::new(LocalHost {
            worktree,
            seller,
            started_on: Mutex::default(),
            runtimes,
            starts: Mutex::default(),
            resumes: Mutex::default(),
            owners: Mutex::default(),
            sink: Mutex::new(mpsc::unbounded_channel().0),
            offline: Mutex::new(setup.offline),
            probes: Mutex::default(),
        });
        let mods = Arc::new(NoMods::default());
        let (engine, task) = Engine::start_with(
            store.clone(),
            host.clone(),
            mods.clone(),
            config.clone(),
            setup.decisions.clone(),
            None,
        )
        .await
        .unwrap();
        engine
            .set_on_limit(Some(run.task_id), setup.on_limit_task)
            .await
            .unwrap();
        engine
            .set_on_limit(None, setup.on_limit_ecosystem)
            .await
            .unwrap();
        if let Some(policy) = &setup.market {
            engine.set_market_policy(policy).await.unwrap();
        }
        engine.start_run(run.id).await.unwrap();
        Self {
            _dir: dir,
            store,
            engine,
            host,
            mods,
            run_id: run.id,
            config,
            task,
        }
    }

    /// Kills the engine task and starts a new one on the same store and host
    /// (the host keeps running, like after a kernel crash).
    async fn restart(&mut self) {
        self.task.abort();
        let _ = (&mut self.task).await;
        let (engine, task) = Engine::start(
            self.store.clone(),
            self.host.clone(),
            self.mods.clone(),
            self.config.clone(),
        )
        .await
        .unwrap();
        (self.engine, self.task) = (engine, task);
    }

    async fn wait_for(&self, done: impl Fn(RunStatus) -> bool) -> Run {
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            let run = self
                .store
                .read()
                .await
                .unwrap()
                .get_run(self.run_id)
                .await
                .unwrap();
            if done(run.status) {
                return run;
            }
            assert!(Instant::now() < deadline, "run still {}", run.status);
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    async fn events(&self) -> Vec<EventRecord> {
        let mut read = self.store.read().await.unwrap();
        read.events_after(Some(self.run_id), 0, 10_000)
            .await
            .unwrap()
    }

    /// `to` of every move of step `key`.
    async fn moves(&self, key: &str) -> Vec<String> {
        self.events()
            .await
            .iter()
            .filter(|e| {
                e.kind == "step_status" && e.step_key.as_ref().is_some_and(|k| k.as_str() == key)
            })
            .map(|e| e.payload["to"].as_str().unwrap().to_owned())
            .collect()
    }

    /// The session ids assigned to the run, in order.
    async fn sessions(&self) -> Vec<String> {
        self.events()
            .await
            .iter()
            .filter(|e| e.kind == "session_assigned")
            .map(|e| e.payload["session_id"].as_str().unwrap().to_owned())
            .collect()
    }

    fn adapters(&self) -> Vec<String> {
        let starts = self.host.starts.lock().unwrap();
        starts.iter().map(|(a, _)| a.clone()).collect()
    }

    fn git(&self, command: &str) -> String {
        sh(&self.host.worktree, command).1
    }

    /// Asserts the succeeded run's hand-off story: one attempt, checkpoint
    /// commit, a stored pack, gates passed after the hand-off.
    async fn assert_handed_off(&self, from: &str) {
        let run = self.wait_for(|s| s.is_terminal()).await;
        assert_eq!(run.status, RunStatus::Succeeded, "{:?}", run.end_reason);
        let steps = self
            .store
            .read()
            .await
            .unwrap()
            .steps_of_run(self.run_id)
            .await
            .unwrap();
        let implement: Vec<_> = steps
            .iter()
            .filter(|s| s.step_key.as_str() == "implement")
            .collect();
        assert_eq!(implement.len(), 1, "the agent step must not re-run");
        assert_eq!(implement[0].attempt, 1);

        let log = self.git("git log --format=%s");
        assert!(
            log.contains(&format!("wip(sun): handoff from {from}")),
            "{log}"
        );
        assert!(self.git("git status --porcelain").trim().is_empty());

        let events = self.events().await;
        let handoff = events
            .iter()
            .find(|e| e.kind == "handoff")
            .expect("handoff event");
        assert_eq!(handoff.payload["from"], from);
        assert_eq!(handoff.payload["checkpoint"], "ok");
        assert_eq!(
            handoff.payload["pack"]["git"]["uncommitted"], true,
            "{:#}",
            handoff.payload["pack"]["git"]
        );
        assert_eq!(handoff.payload["pack"]["from"]["runtime"], from);
        // The full transcript is a blob of this run the new agent can fetch.
        let transcript = handoff.payload["pack"]["transcript_ref"].as_str().unwrap();
        assert!(transcript.starts_with("blob:"), "{transcript}");
        let text = EngineHandle::get(&self.engine, self.run_id, transcript.into())
            .await
            .unwrap();
        assert!(text.contains("assistant: on it"), "{text}");
        let gates: Vec<_> = events.iter().filter(|e| e.kind == "gate_result").collect();
        assert_eq!(gates.len(), 2, "both gates ran");
        assert!(
            gates
                .iter()
                .all(|g| g.seq > handoff.seq && g.payload["result"]["pass"] == true)
        );

        // The pack is delivered as files before the session starts.
        let packs: Vec<_> = events.iter().filter(|e| e.kind == "context_pack").collect();
        assert_eq!(packs.len(), 2, "one pack per session");
        assert!(packs[1].seq > handoff.seq);
        let handoff_md =
            std::fs::read_to_string(self.host.worktree.join(".sun/context/handoff.md")).unwrap();
        assert!(handoff_md.contains("# Hand-off"), "{handoff_md}");
        let pack_md =
            std::fs::read_to_string(self.host.worktree.join(".sun/context/pack.md")).unwrap();
        assert!(pack_md.contains("implement it"), "{pack_md}");
        let starts = self.host.starts.lock().unwrap();
        let (_, envelope) = starts.last().unwrap();
        assert!(envelope.contains("implement it"), "{envelope}");
        assert!(
            envelope.contains("Read `.sun/context/pack.md`"),
            "{envelope}"
        );
        assert!(
            envelope.contains(&format!("**{from}** stopped")),
            "{envelope}"
        );
    }
}

fn script(lines: &[&str]) -> Vec<String> {
    lines.iter().map(|l| (*l).to_owned()).collect()
}

/// Waits for the first event of `kind`.
async fn first_event(h: &Harness, kind: &str) -> EventRecord {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        if let Some(e) = h.events().await.into_iter().find(|e| e.kind == kind) {
            return e;
        }
        assert!(Instant::now() < deadline, "no `{kind}` event");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn agent_get_resolves_run_pointers_and_ask_waits_for_a_human() {
    let slow_done = r#"{"delay_ms":20000,"kind":"done_claimed","summary":"finished"}"#;
    let h = Harness::start(
        "switch",
        &[("fake-limit", vec![script(&[TALK, slow_done, EXIT])])],
    )
    .await;
    let packed = first_event(&h, "context_pack").await;
    let key = StepKey::new("implement").unwrap();
    let handle: Arc<dyn EngineHandle> = Arc::new(h.engine.clone());

    // sun.get: this run's pack blob resolves; others don't.
    let id = serde_json::from_value(packed.payload["context_pack_id"].clone()).unwrap();
    let mut read = h.store.read().await.unwrap();
    let row = sun_store::ContextRepo::get_context_pack(&mut read, id)
        .await
        .unwrap();
    drop(read);
    let pointer = format!("blob:{}", row.pack_hash.as_str());
    let text = handle.get(h.run_id, pointer.clone()).await.unwrap();
    assert!(text.contains("implement it"), "{text}");
    let other = h.store.blobs().put(b"not yours".to_vec()).await.unwrap();
    for bad in [
        format!("blob:{}", other.as_str()),
        "artifact:01J00000000000000000000000".into(),
        "memory:x".into(),
        ".sun/context/pack.md".into(),
    ] {
        let err = handle.get(h.run_id, bad.clone()).await.unwrap_err();
        assert!(matches!(err, Error::NotFound(_)), "{bad}: {err}");
    }
    let err = handle.get(RunId::generate(), pointer).await.unwrap_err();
    assert!(matches!(err, Error::NotFound(_)), "another run: {err}");

    // sun.ask: the step waits for a human; only an offered option answers.
    let asking = handle.clone();
    let (run_id, step_key) = (h.run_id, key.clone());
    let answer = tokio::spawn(async move {
        asking
            .ask(
                run_id,
                step_key,
                "Which db?".into(),
                vec!["sqlite".into(), "pg".into()],
            )
            .await
    });
    let question = first_event(&h, "question").await;
    assert_eq!(question.payload["question"], "Which db?");
    let qid = serde_json::from_value(question.payload["question_id"].clone()).unwrap();
    assert_eq!(
        h.moves("implement").await.last().unwrap(),
        "waiting_approval"
    );
    let err = h.engine.answer(h.run_id, qid, "mysql".into()).await;
    assert!(matches!(err, Err(Error::InvalidInput(_))), "{err:?}");
    h.engine
        .answer(h.run_id, qid, "sqlite".into())
        .await
        .unwrap();
    assert_eq!(answer.await.unwrap().unwrap(), "sqlite");
    assert_eq!(h.moves("implement").await.last().unwrap(), "running");
    let err = h.engine.answer(h.run_id, qid, "pg".into()).await;
    assert!(
        matches!(err, Err(Error::InvalidInput(_))),
        "closed: {err:?}"
    );
    let notes = handle.notes(h.run_id).await.unwrap();
    assert_eq!(notes.len(), 1);
    assert_eq!(notes[0].body, "Which db?");

    // A manual switch from `running` is a hand-off, not an interruption.
    h.engine
        .switch_runtime(h.run_id, key, "fake-ok".into())
        .await
        .unwrap();
    let moves = h.moves("implement").await;
    assert!(
        moves
            .windows(3)
            .any(|w| w == ["running", "handing_off", "dispatched"]),
        "{moves:?}"
    );
    assert!(!moves.contains(&"interrupted".into()), "{moves:?}");
    h.engine.cancel(h.run_id).await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn context_pack_files_are_written_before_the_session_starts() {
    let h = Harness::start("switch", &[("fake-limit", vec![script(&[TALK, EXIT])])]).await;
    let deadline = Instant::now() + Duration::from_secs(20);
    let event = loop {
        let events = h.events().await;
        if let Some(e) = events.into_iter().find(|e| e.kind == "context_pack") {
            break e;
        }
        assert!(Instant::now() < deadline, "no context_pack event");
        tokio::time::sleep(Duration::from_millis(20)).await;
    };
    // `LocalHost::start_session` asserts `pack.md` exists before the start.
    let id = serde_json::from_value(event.payload["context_pack_id"].clone()).unwrap();
    let row = {
        let mut read = h.store.read().await.unwrap();
        sun_store::ContextRepo::get_context_pack(&mut read, id)
            .await
            .unwrap()
    };
    let pack_md = std::fs::read_to_string(h.host.worktree.join(sun_context::PACK_PATH)).unwrap();
    let blob = h.store.blobs().get(&row.pack_hash).await.unwrap();
    assert_eq!(
        String::from_utf8(blob).unwrap(),
        pack_md,
        "stored = delivered"
    );
    assert!(
        event.payload["summary"]
            .as_str()
            .unwrap()
            .contains("tokens")
    );
    assert!(pack_md.contains("implement it"), "{pack_md}");
    let root = &h.host.worktree;
    assert!(root.join(".sun/context/.gitignore").exists());
    assert!(root.join(sun_context::PROGRESS_PATH).exists());
    assert!(
        !root.join(sun_context::HANDOFF_PATH).exists(),
        "no hand-off yet"
    );
    let starts = h.host.starts.lock().unwrap();
    let (_, envelope) = &starts[0];
    assert!(envelope.contains("implement it"), "{envelope}");
    assert!(
        envelope.contains("Read `.sun/context/pack.md`"),
        "{envelope}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn failover_switches_to_the_next_runtime_mid_step() {
    let limit = limit_hit(Duration::from_secs(3600));
    let h = Harness::start(
        "switch",
        &[
            ("fake-limit", vec![script(&[TALK, EDIT, &limit, EXIT])]),
            ("fake-ok", vec![script(&[TALK, DONE, EXIT])]),
        ],
    )
    .await;
    h.assert_handed_off("fake-limit").await;
    assert_eq!(h.adapters(), ["fake-limit", "fake-ok"]);
    assert_eq!(
        h.moves("implement").await,
        [
            "queued",
            "dispatched",
            "running",
            "limit_hit",
            "handing_off",
            "dispatched",
            "running",
            "verifying",
            "succeeded"
        ]
    );
    let events = h.events().await;
    let cooling = events.iter().find(|e| e.kind == "runtime_cooling").unwrap();
    assert_eq!(cooling.payload["runtime"], "fake-limit");
    // The cooldown is stored, so it survives a restart.
    let mut read = h.store.read().await.unwrap();
    let stored = sun_store::RuntimeRepo::cooldowns(&mut read).await.unwrap();
    assert_eq!(stored.len(), 1);
    assert_eq!(stored[0].0, "fake-limit");
    assert!(stored[0].1 > chrono::Utc::now());
}

#[tokio::test(flavor = "multi_thread")]
async fn failover_asks_the_decision_engine_to_switch_or_pause() {
    let limit = limit_hit(Duration::from_secs(3600));
    let scripts = [
        ("fake-limit", vec![script(&[TALK, EDIT, &limit, EXIT])]),
        ("fake-ok", vec![script(&[TALK, DONE, EXIT])]),
    ];
    let setup = Setup {
        decisions: Some(support::systemone(
            r#"{"answers": {"decision": {"choice": "pause", "answer_confidence": 0.9}},
                "model": "laya-1"}"#,
        )),
        ..Setup::default()
    };
    let h = Harness::with("switch", &scripts, setup).await;
    let paused = h.wait_for(|s| s == RunStatus::Paused).await;
    let why = paused.end_reason.unwrap_or_default();
    assert!(why.contains("decision engine chose to pause"), "{why}");
    let events = h.events().await;
    let decision = events.iter().find(|e| e.kind == "decision").unwrap();
    assert_eq!(decision.payload["decides"], "failover");
    assert_eq!(decision.payload["acted"], true);
    assert_eq!(decision.payload["rule"], json!({"choice": "fake-ok"}));
    assert_eq!(
        decision.payload["question"]["criteria"]["fake-ok"],
        "switch to `fake-ok` now"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn failover_pauses_then_a_manual_switch_continues() {
    let limit = limit_hit(Duration::from_secs(3600));
    let h = Harness::start(
        "pause_and_notify",
        &[
            ("fake-limit", vec![script(&[TALK, EDIT, &limit, EXIT])]),
            ("fake-ok", vec![script(&[TALK, DONE, EXIT])]),
        ],
    )
    .await;
    let paused = h.wait_for(|s| s == RunStatus::Paused).await;
    assert!(
        paused
            .end_reason
            .as_deref()
            .unwrap_or_default()
            .contains("fake-limit"),
        "{:?}",
        paused.end_reason
    );
    let key = StepKey::new("implement").unwrap();
    let refused = h
        .engine
        .switch_runtime(h.run_id, key.clone(), "gpt-nowhere".into())
        .await;
    assert!(
        matches!(refused, Err(Error::InvalidInput(_))),
        "{refused:?}"
    );
    h.engine
        .switch_runtime(h.run_id, key, "fake-ok".into())
        .await
        .unwrap();
    h.assert_handed_off("fake-limit").await;
    assert_eq!(h.adapters(), ["fake-limit", "fake-ok"]);
    let events = h.events().await;
    let handoff = events.iter().find(|e| e.kind == "handoff").unwrap();
    assert_eq!(handoff.payload["reason"], "manual_switch");
}

#[tokio::test(flavor = "multi_thread")]
async fn failover_waits_for_the_reset_then_resumes_on_the_same_runtime() {
    let limit = limit_hit(Duration::from_secs(1));
    let h = Harness::start(
        "wait_until_reset",
        &[(
            "fake-limit",
            vec![
                script(&[TALK, EDIT, &limit, EXIT]),
                script(&[TALK, DONE, EXIT]),
            ],
        )],
    )
    .await;
    h.assert_handed_off("fake-limit").await;
    assert_eq!(h.adapters(), ["fake-limit", "fake-limit"]);
    // The same runtime resumes its own session first.
    let first = h.sessions().await[0].clone();
    assert_eq!(*h.host.resumes.lock().unwrap(), [None, Some(first)]);
    let starts = h.host.starts.lock().unwrap();
    assert!(
        starts[1].1.contains("Sun resumed this session"),
        "{}",
        starts[1].1
    );
}

const RESUME_FAILS: &str = r#"{"kind":"error","message":"No conversation found"}"#;
const EXIT_1: &str = r#"{"kind":"exited","code":1}"#;

#[tokio::test(flavor = "multi_thread")]
async fn failover_a_failed_resume_falls_back_to_a_fresh_session_with_a_notice() {
    let limit = limit_hit(Duration::from_secs(1));
    let h = Harness::start(
        "wait_until_reset",
        &[(
            "fake-limit",
            vec![
                script(&[TALK, EDIT, &limit, EXIT]),
                script(&[RESUME_FAILS, EXIT_1]),
                script(&[TALK, DONE, EXIT]),
            ],
        )],
    )
    .await;
    let run = h.wait_for(|s| s.is_terminal()).await;
    assert_eq!(run.status, RunStatus::Succeeded, "{:?}", run.end_reason);
    let sessions = h.sessions().await;
    let resumes = h.host.resumes.lock().unwrap().clone();
    assert_eq!(resumes, [None, Some(sessions[0].clone()), None]);
    let notice = first_event(&h, "resume_failed").await;
    assert_eq!(notice.payload["native_id"], sessions[0].as_str());
    assert_eq!(notice.payload["error"], "No conversation found");
    // The fresh session still gets the hand-off pack, without the notice.
    let starts = h.host.starts.lock().unwrap();
    assert!(
        starts[2].1.contains("**fake-limit** stopped"),
        "{}",
        starts[2].1
    );
    assert!(!starts[2].1.contains("Sun resumed"), "{}", starts[2].1);
}

#[tokio::test(flavor = "multi_thread")]
async fn failover_a_failed_start_retries_until_the_host_is_back() {
    let setup = Setup {
        offline: 2,
        ..Setup::default()
    };
    let h = Harness::with(
        "switch",
        &[("fake-limit", vec![script(&[TALK, EDIT, DONE, EXIT])])],
        setup,
    )
    .await;
    // No hand-off commits the work, so only the agent step is checked.
    h.wait_for(|s| s.is_terminal()).await;
    assert_eq!(h.moves("implement").await.last().unwrap(), "succeeded");
    let failed = h.events().await;
    let failed: Vec<_> = failed
        .iter()
        .filter(|e| e.kind == "dispatch_failed")
        .collect();
    assert_eq!(failed.len(), 2);
    assert_eq!(h.adapters(), ["fake-limit"]);
}

#[tokio::test(flavor = "multi_thread")]
async fn failover_a_host_that_stays_offline_fails_the_run_with_a_clear_reason() {
    let setup = Setup {
        offline: usize::MAX,
        ..Setup::default()
    };
    let h = Harness::with(
        "switch",
        &[("fake-limit", vec![script(&[TALK, DONE, EXIT])])],
        setup,
    )
    .await;
    let run = h.wait_for(|s| s.is_terminal()).await;
    assert_eq!(run.status, RunStatus::Failed);
    let reason = run.end_reason.unwrap_or_default();
    assert!(reason.contains("unreachable after 3 tries"), "{reason}");
    assert!(reason.contains("host is offline"), "{reason}");
    assert_eq!(h.moves("implement").await.last().unwrap(), "failed");
}

#[tokio::test(flavor = "multi_thread")]
async fn failover_skips_unready_runtimes_with_a_reason() {
    let readiness = |logged_in: bool| {
        serde_json::json!({"installed": true, "version": "1", "logged_in": logged_in,
            "detail": (!logged_in).then_some("run `fake login`")})
    };
    let setup = Setup {
        capabilities: Some(serde_json::json!({
            "os": "macos", "arch": "aarch64", "cpus": 8,
            "runtimes": [
                {"adapter": "fake-limit", "readiness": readiness(false)},
                {"adapter": "fake-ok", "readiness": readiness(true)},
            ],
        })),
        ..Setup::default()
    };
    let h = Harness::with(
        "switch",
        &[
            ("fake-limit", vec![script(&[TALK, EDIT, DONE, EXIT])]),
            ("fake-ok", vec![script(&[TALK, EDIT, DONE, EXIT])]),
        ],
        setup,
    )
    .await;
    h.wait_for(|s| s.is_terminal()).await;
    assert_eq!(h.moves("implement").await.last().unwrap(), "succeeded");
    assert_eq!(h.adapters(), ["fake-ok"]);
    let skipped = first_event(&h, "runtime_skipped").await;
    assert_eq!(skipped.payload["runtime"], "fake-limit");
    assert_eq!(
        skipped.payload["reason"],
        "`fake-limit` is not logged in: run `fake login`"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn failover_a_lost_session_resumes_once_its_host_answers_again() {
    let slow_done = r#"{"delay_ms":60000,"kind":"done_claimed","summary":"finished"}"#;
    let mut h = Harness::start(
        "switch",
        &[(
            "fake-limit",
            vec![
                script(&[TALK, EDIT, slow_done, EXIT]),
                script(&[TALK, DONE, EXIT]),
            ],
        )],
    )
    .await;
    first_event(&h, "context_pack").await;
    // The kernel restarts; its host answers only the second probe, and the
    // session is gone by then.
    *h.host.probes.lock().unwrap() = vec![Err(Error::Unavailable("host offline".into()))];
    h.restart().await;
    let run = h.wait_for(|s| s.is_terminal()).await;
    assert_eq!(run.status, RunStatus::Succeeded, "{:?}", run.end_reason);
    let events = h.events().await;
    assert!(events.iter().any(|e| e.kind == "host_unreachable"));
    let handoff = events.iter().find(|e| e.kind == "handoff").unwrap();
    assert_eq!(handoff.payload["reason"], "host_lost");
    let first = h.sessions().await[0].clone();
    assert_eq!(*h.host.resumes.lock().unwrap(), [None, Some(first)]);
    let moves = h.moves("implement").await;
    assert!(
        moves
            .windows(3)
            .any(|w| w == ["running", "interrupted", "dispatched"]),
        "{moves:?}"
    );
}

fn usage(context_used: u32) -> String {
    format!(
        r#"{{"kind":"usage","input":10,"output":5,"cost_usd":null,"context_used":{context_used}}}"#
    )
}

fn tool_call(hash: u64) -> String {
    format!(r#"{{"kind":"tool_call","name":"Bash","args_hash":{hash},"summary":"ls"}}"#)
}

const TOOL_RESULT: &str = r#"{"kind":"tool_result","ok":true,"summary":"ok"}"#;
/// Keeps the session open until it is stopped.
const LINGER: &str = r#"{"delay_ms":20000,"kind":"exited","code":0}"#;

#[tokio::test(flavor = "multi_thread")]
async fn context_rotation_hands_off_to_a_fresh_session_at_the_next_tool_result() {
    let pin = "Never touch the billing schema.";
    let (low, high, fresh) = (usage(500), usage(850), usage(120));
    let (call_1, call_2) = (tool_call(1), tool_call(2));
    let h = Harness::with(
        "switch",
        &[(
            "fake-limit",
            vec![
                // 50%, a tool call, 85% (armed), more talk — not a safe point —
                // then the tool call ends: rotate there.
                script(&[
                    TALK,
                    EDIT,
                    &low,
                    &call_1,
                    TOOL_RESULT,
                    &high,
                    TALK,
                    &call_2,
                    TOOL_RESULT,
                    LINGER,
                ]),
                script(&[TALK, &fresh, DONE, EXIT]),
            ],
        )],
        Setup {
            context_windows: vec![("fake-limit", 1000)],
            pinned: vec![pin.into()],
            ..Setup::default()
        },
    )
    .await;
    h.assert_handed_off("fake-limit").await;
    assert_eq!(h.adapters(), ["fake-limit", "fake-limit"]);
    assert_eq!(
        h.moves("implement").await,
        [
            "queued",
            "dispatched",
            "running",
            "handing_off",
            "dispatched",
            "running",
            "verifying",
            "succeeded"
        ]
    );
    // A fresh session of the same runtime, not a resume of the full one.
    assert_eq!(*h.host.resumes.lock().unwrap(), [None, None]);

    let events = h.events().await;
    let at = events.iter().position(|e| e.kind == "rotation").unwrap();
    let rotation = &events[at];
    assert_eq!(rotation.payload["runtime"], "fake-limit");
    assert_eq!(rotation.payload["context_used"], 850);
    assert_eq!(rotation.payload["context_window"], 1000);
    assert_eq!(rotation.payload["percent"], 85);
    // Waited for the end of the tool call after the threshold was passed.
    assert_eq!(events[at - 1].kind, "tool_result");
    let results = events[..at]
        .iter()
        .filter(|e| e.kind == "tool_result")
        .count();
    assert_eq!(results, 2);
    let handoff = events.iter().find(|e| e.kind == "handoff").unwrap();
    assert_eq!(handoff.payload["reason"], "context_rotation");
    assert!(handoff.seq > rotation.seq);
    assert_eq!(
        events.iter().filter(|e| e.kind == "rotation").count(),
        1,
        "the fresh session is below the threshold"
    );
    // The meter's inputs: every session journals its window and threshold.
    let assigned: Vec<_> = events
        .iter()
        .filter(|e| e.kind == "session_assigned")
        .collect();
    assert_eq!(assigned.len(), 2);
    assert!(
        assigned
            .iter()
            .all(|e| e.payload["context_window"] == 1000 && e.payload["rotate_at_percent"] == 80)
    );

    // Pinned constraints survive into both sessions' envelopes and packs.
    let starts = h.host.starts.lock().unwrap();
    assert!(starts.iter().all(|(_, envelope)| envelope.contains(pin)));
    let pack_md = std::fs::read_to_string(h.host.worktree.join(".sun/context/pack.md")).unwrap();
    assert!(
        pack_md.contains(pin) && pack_md.contains("# Hand-off"),
        "{pack_md}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn context_rotation_needs_a_known_window() {
    let high = usage(990);
    let call = tool_call(1);
    let h = Harness::with(
        "switch",
        &[(
            "fake-limit",
            vec![script(&[TALK, EDIT, &high, &call, TOOL_RESULT, DONE, EXIT])],
        )],
        Setup::default(),
    )
    .await;
    // The gate fails (nothing committed the work): only the rotation matters.
    h.wait_for(|s| s.is_terminal()).await;
    assert!(!h.events().await.iter().any(|e| e.kind == "rotation"));
    assert_eq!(h.adapters(), ["fake-limit"]);
}

/// A fresh session that starts past the threshold (the pack fills a tiny
/// window) is never rotated again: no rotation after every tool call.
#[tokio::test(flavor = "multi_thread")]
async fn context_rotation_skips_a_fresh_session_that_starts_full() {
    let (high, still_high) = (usage(850), usage(900));
    let (call_1, call_2, call_3) = (tool_call(1), tool_call(2), tool_call(3));
    let h = Harness::with(
        "switch",
        &[(
            "fake-limit",
            vec![
                script(&[TALK, EDIT, &high, &call_1, TOOL_RESULT, LINGER]),
                script(&[
                    TALK,
                    &still_high,
                    &call_2,
                    TOOL_RESULT,
                    &still_high,
                    &call_3,
                    TOOL_RESULT,
                    DONE,
                    EXIT,
                ]),
            ],
        )],
        Setup {
            context_windows: vec![("fake-limit", 1000)],
            ..Setup::default()
        },
    )
    .await;
    h.wait_for(|s| s.is_terminal()).await;
    let events = h.events().await;
    let count = |kind: &str| events.iter().filter(|e| e.kind == kind).count();
    assert_eq!(count("rotation"), 1, "rotated once, then guarded");
    assert_eq!(count("rotation_skipped"), 1);
    assert_eq!(h.adapters(), ["fake-limit", "fake-limit"]);
}

#[tokio::test(flavor = "multi_thread")]
async fn failover_the_pr_body_names_each_runtime_and_who_paid() {
    let limit = limit_hit(Duration::from_secs(3600));
    let h = Harness::with(
        "switch",
        &[
            ("fake-limit", vec![script(&[TALK, EDIT, &limit, EXIT])]),
            ("fake-ok", vec![script(&[TALK, DONE, EXIT])]),
        ],
        Setup {
            pr: true,
            ..Setup::default()
        },
    )
    .await;
    let deadline = Instant::now() + Duration::from_secs(20);
    let body = loop {
        let mut tx = h.store.begin().await.unwrap();
        let mut rows = Vec::new();
        for status in [
            OutboxStatus::Pending,
            OutboxStatus::Unknown,
            OutboxStatus::Failed,
        ] {
            rows.extend(tx.outbox_with_status(status, 10).await.unwrap());
        }
        drop(tx);
        if let Some(row) = rows.iter().find(|r| r.target == "github.open_pr") {
            break row.payload["body"].as_str().unwrap().to_owned();
        }
        assert!(Instant::now() < deadline, "no open_pr row");
        tokio::time::sleep(Duration::from_millis(20)).await;
    };
    assert!(
        body.contains(
            "- `implement`: fake-limit (your subscription) → fake-ok (your subscription)"
        ),
        "{body}"
    );
    // The timeline names who paid for each session too.
    let assigned = first_event(&h, "session_assigned").await;
    assert_eq!(assigned.payload["source"], "byo_runtime");
}

#[tokio::test(flavor = "multi_thread")]
async fn failover_a_task_pause_beats_the_workflow_switch() {
    let limit = limit_hit(Duration::from_secs(3600));
    let h = Harness::with(
        "switch",
        &[
            ("fake-limit", vec![script(&[TALK, EDIT, &limit, EXIT])]),
            ("fake-ok", vec![script(&[TALK, DONE, EXIT])]),
        ],
        Setup {
            on_limit_task: Some(OnLimit::PauseAndNotify),
            // The workflow's own `switch` beats the ecosystem default.
            on_limit_ecosystem: Some(OnLimit::WaitUntilReset),
            ..Setup::default()
        },
    )
    .await;
    let paused = h.wait_for(|s| s == RunStatus::Paused).await;
    let reason = paused.end_reason.unwrap_or_default();
    assert!(reason.contains("fake-limit"), "{reason}");
    assert_eq!(h.adapters(), ["fake-limit"], "no switch happened");
    // The pause waits for a person to pick: the task needs them.
    let task = h.engine.task(paused.task_id).await.unwrap();
    assert_eq!(task.status, sun_core::TaskStatus::NeedsYou);
    assert_eq!(
        h.engine.on_limit(None).await.unwrap(),
        Some(OnLimit::WaitUntilReset)
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn failover_switches_back_after_the_cooldown_with_a_hand_off() {
    // `fake-ok` starts after the limit is handled, so its tool result comes
    // at least 3 s after the limit: always past the cooldown. The engine only
    // has to handle the limit within those 3 s for `fake-ok` to be picked.
    let limit = limit_hit(Duration::from_secs(3));
    let call = tool_call(1);
    let late_result = r#"{"delay_ms":3000,"kind":"tool_result","ok":true,"summary":"ok"}"#;
    let h = Harness::with(
        "switch",
        &[
            (
                "fake-limit",
                vec![
                    script(&[TALK, EDIT, &limit, EXIT]),
                    script(&[TALK, DONE, EXIT]),
                ],
            ),
            ("fake-ok", vec![script(&[TALK, &call, late_result, LINGER])]),
        ],
        Setup {
            runtime_extra: "switch_back: true",
            ..Setup::default()
        },
    )
    .await;
    let run = h.wait_for(|s| s.is_terminal()).await;
    assert_eq!(run.status, RunStatus::Succeeded, "{:?}", run.end_reason);
    assert_eq!(h.adapters(), ["fake-limit", "fake-ok", "fake-limit"]);
    let events = h.events().await;
    let handoffs: Vec<_> = events.iter().filter(|e| e.kind == "handoff").collect();
    assert_eq!(handoffs.len(), 2);
    let back = &handoffs[1].payload;
    assert_eq!(
        (&back["from"], &back["to"]),
        (&"fake-ok".into(), &"fake-limit".into())
    );
    assert_eq!(back["reason"], "switch_back");
    assert_eq!(back["pack"]["from"]["reason"], "switch_back");
}

#[tokio::test(flavor = "multi_thread")]
async fn failover_review_after_failover_waits_for_a_listed_approver() {
    let limit = limit_hit(Duration::from_secs(3600));
    let h = Harness::with(
        "switch",
        &[
            ("fake-limit", vec![script(&[TALK, EDIT, &limit, EXIT])]),
            ("fake-ok", vec![script(&[TALK, DONE, EXIT])]),
        ],
        Setup {
            runtime_extra: "review_after_failover: [user:lead]",
            ..Setup::default()
        },
    )
    .await;
    let review = first_event(&h, "failover_review").await;
    assert_eq!(
        review.payload["runtimes"],
        serde_json::json!(["fake-limit", "fake-ok"])
    );
    assert_eq!(
        h.moves("implement").await.last().unwrap(),
        "waiting_approval"
    );
    let key = StepKey::new("implement").unwrap();
    let who = |user: &str| Principal {
        user: user.into(),
        roles: vec![],
    };
    let refused = h
        .engine
        .decide(h.run_id, key.clone(), true, who("mallory"))
        .await;
    assert!(matches!(refused, Err(Error::Forbidden(_))), "{refused:?}");
    h.engine
        .decide(h.run_id, key, true, who("lead"))
        .await
        .unwrap();
    let run = h.wait_for(|s| s.is_terminal()).await;
    assert_eq!(run.status, RunStatus::Succeeded, "{:?}", run.end_reason);
}

/// A chain `[fake-limit, market:fake-ok]` within a policy with a failover cap.
fn market_setup() -> Setup {
    Setup {
        pr: true,
        chain: "fake-limit, market:fake-ok",
        market: Some(MarketPolicy {
            failover_max_price_points: Some(50),
            ..MarketPolicy::default()
        }),
        seller: true,
        ..Setup::default()
    }
}

impl Harness {
    async fn task_id(&self) -> TaskId {
        let mut read = self.store.read().await.unwrap();
        read.get_run(self.run_id).await.unwrap().task_id
    }

    /// Waits until step `key` has moved to `status`.
    async fn wait_step(&self, key: &str, status: &str) {
        let deadline = Instant::now() + Duration::from_secs(20);
        while !self.moves(key).await.iter().any(|m| m == status) {
            assert!(Instant::now() < deadline, "{key} never {status}");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }
}

/// Mid-run: a running step whose runtime hits its limit fails over to
/// `market:fake-ok`. The task is held for a lease; once leased and the
/// seller's host joins, the checkpoint branch is pushed, the seller's host
/// checks it out, and the leased adapter continues with the hand-off pack.
#[tokio::test(flavor = "multi_thread")]
async fn running_step_fails_over_to_market_runtime() {
    let limit = limit_hit(Duration::from_secs(3600));
    let h = Harness::with(
        "switch",
        &[
            ("fake-limit", vec![script(&[TALK, EDIT, &limit, EXIT])]),
            ("fake-ok", vec![script(&[TALK, DONE, EXIT])]),
        ],
        market_setup(),
    )
    .await;
    let task = h.task_id().await;
    let failover = first_event(&h, sun_engine::MARKET_FAILOVER).await;
    assert_eq!(failover.payload["runtime"], "fake-ok");
    assert_eq!(failover.payload["max_price_points"], 50);
    let held = h.engine.task_lease(task).await.unwrap().unwrap();
    assert_eq!(
        (held.lease_id, held.runtime.as_deref()),
        (None, Some("fake-ok"))
    );

    // Leased, but the seller's host hasn't joined: the step waits.
    let vm = sun_core::ExecEnvSpec::Vm(sun_core::VmSpec {
        template: "default".into(),
        memory_mb: None,
        cpus: None,
        network: Some(vec!["api.anthropic.com".into()]),
        keep: false,
    });
    h.engine.lease_task(task, "lea_1", &vm).await.unwrap();
    h.wait_step("implement", "handing_off").await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(
        h.adapters(),
        ["fake-limit"],
        "no session before the seller joins"
    );

    // The seller's host joins: the run moves there and finishes the step.
    let (seller, seller_dir) = h.host.seller.clone().unwrap();
    h.engine.bind_market_host(task, seller).await.unwrap();
    h.wait_step("verify", "succeeded").await;
    assert_eq!(h.adapters(), ["fake-limit", "fake-ok"]);
    assert_eq!(*h.host.started_on.lock().unwrap(), [h.config.host, seller]);
    let log = sh(&seller_dir, "git log --format=%s").1;
    assert!(log.contains("wip(sun): handoff from fake-limit"), "{log}");
    assert!(seller_dir.join("work.txt").is_file(), "the work moved");
    let pack = std::fs::read_to_string(seller_dir.join(".sun/context/handoff.md")).unwrap();
    assert!(pack.contains("# Hand-off"), "{pack}");
    let handoff = first_event(&h, "handoff").await;
    assert_eq!(handoff.payload["to"], "market:fake-ok");
    assert_eq!(handoff.payload["checkpoint"], "ok");
    let mut read = h.store.read().await.unwrap();
    let steps = read.steps_of_run(h.run_id).await.unwrap();
    drop(read);
    let implement = steps.iter().filter(|s| s.step_key.as_str() == "implement");
    assert_eq!(implement.count(), 1, "the agent step must not re-run");
    let events = h.events().await;
    let gates: Vec<_> = events.iter().filter(|e| e.kind == "gate_result").collect();
    assert!(!gates.is_empty());
    assert!(gates.iter().all(|g| g.payload["result"]["pass"] == true));
}

/// A refused lease (the hold released) hands the step back to its own
/// runtime once that runtime resets.
#[tokio::test(flavor = "multi_thread")]
async fn failover_a_refused_market_lease_hands_back_after_the_reset() {
    let limit = limit_hit(Duration::from_secs(1));
    let h = Harness::with(
        "switch",
        &[(
            "fake-limit",
            vec![
                script(&[TALK, EDIT, &limit, EXIT]),
                script(&[TALK, DONE, EXIT]),
            ],
        )],
        market_setup(),
    )
    .await;
    let task = h.task_id().await;
    first_event(&h, sun_engine::MARKET_FAILOVER).await;
    h.engine.release_lease_hold(task).await.unwrap();
    h.wait_step("verify", "succeeded").await;
    assert_eq!(h.adapters(), ["fake-limit", "fake-limit"]);
    assert_eq!(h.engine.task_lease(task).await.unwrap(), None);
    let started_on = h.host.started_on.lock().unwrap().clone();
    assert!(
        started_on.iter().all(|host| *host == h.config.host),
        "never moved"
    );
    let handoff = first_event(&h, "handoff").await;
    assert_eq!(handoff.payload["to"], "fake-limit");
}

/// The seller joins but the checkpoint push fails. The filled lease is
/// released in the failure's transaction (Sun Cloud refunds its escrow
/// through the outbox, once per lease), the task's own exec env is back, and
/// the step continues on the buyer's host after its runtime resets.
#[tokio::test(flavor = "multi_thread")]
async fn failover_a_failed_checkpoint_push_releases_the_paid_lease() {
    let limit = limit_hit(Duration::from_secs(1));
    let h = Harness::with(
        "switch",
        &[
            (
                "fake-limit",
                vec![
                    script(&[TALK, EDIT, &limit, EXIT]),
                    script(&[TALK, DONE, EXIT]),
                ],
            ),
            ("fake-ok", vec![script(&[TALK, DONE, EXIT])]),
        ],
        market_setup(),
    )
    .await;
    let task = h.task_id().await;
    first_event(&h, sun_engine::MARKET_FAILOVER).await;
    let vm = sun_core::ExecEnvSpec::Vm(sun_core::VmSpec {
        template: "default".into(),
        memory_mb: None,
        cpus: None,
        network: Some(vec!["api.anthropic.com".into()]),
        keep: false,
    });
    h.engine.lease_task(task, "lea_1", &vm).await.unwrap();
    // The checkpoint can't reach origin.
    h.git("git remote set-url origin ../gone.git");
    let (seller, _) = h.host.seller.clone().unwrap();
    h.engine.bind_market_host(task, seller).await.unwrap();

    h.wait_step("verify", "succeeded").await;
    let failed = first_event(&h, "market_move_failed").await;
    assert_eq!(failed.payload["to"], "market:fake-ok");
    assert_eq!(h.engine.task_lease(task).await.unwrap(), None, "released");
    let mut read = h.store.read().await.unwrap();
    let env = read
        .setting(&format!("task_exec_env.{task}"))
        .await
        .unwrap();
    drop(read);
    assert_eq!(env, Some(Value::Null), "the lease's vm is gone");
    // Sun Cloud was asked once to release (refund) the lease.
    let delivered = first_event(&h, "outbox_delivered").await;
    assert_eq!(delivered.payload["target"], "sun_cloud.release_lease");
    let calls = h.mods.0.lock().unwrap().clone();
    assert_eq!(calls.len(), 1, "{calls:?}");
    assert_eq!(calls[0].method, sun_engine::RELEASE_LEASE);
    assert_eq!(calls[0].params["lease_id"], "lea_1");
    assert_eq!(
        calls[0].idem_key.as_ref().unwrap().as_str(),
        "market_release:lea_1"
    );
    // The step finished on the buyer's own host and runtime.
    assert_eq!(h.adapters(), ["fake-limit", "fake-limit"]);
    let started_on = h.host.started_on.lock().unwrap().clone();
    assert!(started_on.iter().all(|host| *host == h.config.host));
    let handoff = first_event(&h, "handoff").await;
    assert_eq!(handoff.payload["to"], "fake-limit");
}
