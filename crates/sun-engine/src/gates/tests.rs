//! Gates and approvals end to end: the real engine and store, a host fake
//! that runs `exec` with `sh` in a temp git worktree, and a scripted agent.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::process::Command as Process;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use serde_json::{Value, json};
use sun_core::host_protocol::{ExecOutput, ExecRequest, Worktree};
use sun_core::model::{Budget, Run, Step, Task, Workflow as WorkflowRow};
use sun_core::{
    BoxStream, EngineHandle, HostEvent, HostId, HostLink, ModCall, ModLink, Result, RunId,
    RunStatus, SessionHandle, SessionId, SessionSpec, Steer, StepKey, StepStatus, StopMode,
    SunEvent, TaskId, TaskStatus, WorkflowId,
};
use sun_store::{EventLog, RepoRepo, RunRepo, Store, TaskRepo, WorkflowRepo};
use tokio::sync::mpsc;

use super::*;
use crate::{Engine, EngineConfig, Principal};

// ---------------------------------------------------------------- fakes

/// What the fake agent does in the worktree given its envelope.
type Agent = Box<dyn Fn(&Path, &str) + Send + Sync>;

/// One task's host: worktrees under `work/`, `exec` through `sh -c`, and
/// agent sessions that run [`Agent`] then claim done and exit.
pub(crate) struct LocalHost {
    work: PathBuf,
    agent: Agent,
    envelopes: Mutex<Vec<String>>,
    /// Each started session's permission mode, in start order.
    pub(crate) modes: Mutex<Vec<Option<String>>>,
    /// Tasks whose worktrees were removed (recorded only).
    pub(crate) removed: Mutex<Vec<TaskId>>,
    /// While set, a started session says one line and waits before claiming
    /// done (to steer, pause or switch it); a session stopped meanwhile
    /// says nothing more.
    pub(crate) hold: Arc<AtomicBool>,
    stopped: Arc<Mutex<HashSet<SessionId>>>,
    sink: Arc<Mutex<Option<mpsc::UnboundedSender<HostEvent>>>>,
}

impl LocalHost {
    pub(crate) fn new(
        dir: &Path,
        agent: impl Fn(&Path, &str) + Send + Sync + 'static,
    ) -> Arc<Self> {
        Arc::new(Self {
            work: dir.join("work"),
            agent: Box::new(agent),
            envelopes: Mutex::default(),
            modes: Mutex::default(),
            removed: Mutex::default(),
            hold: Arc::default(),
            stopped: Arc::default(),
            sink: Arc::default(),
        })
    }

    /// The single task's worktree: a git repo whose `origin/main` is its
    /// first commit, like a host worktree.
    fn worktree(&self) -> PathBuf {
        let dir = self.work.join("task");
        if !dir.exists() {
            std::fs::create_dir_all(&dir).unwrap();
            let script = "git init -q -b main && git -c user.email=t@t -c user.name=t \
                          commit -q --allow-empty -m base && git update-ref refs/remotes/origin/main HEAD";
            assert!(sh(&dir, script).0 == Some(0));
        }
        dir
    }
}

fn sh(cwd: &Path, command: &str) -> (Option<i32>, String) {
    sh_env(cwd, command, &Default::default())
}

fn sh_env(
    cwd: &Path,
    command: &str,
    env: &std::collections::BTreeMap<String, String>,
) -> (Option<i32>, String) {
    let out = Process::new("sh")
        .arg("-c")
        .arg(command)
        .envs(env)
        .current_dir(cwd)
        .output()
        .unwrap();
    let text =
        String::from_utf8_lossy(&out.stdout).into_owned() + &String::from_utf8_lossy(&out.stderr);
    // Like a real host (sun-host `process.rs`): only the last 64 KiB come back.
    let cut = text.len().saturating_sub(64 * 1024);
    let cut = (cut..=text.len())
        .find(|i| text.is_char_boundary(*i))
        .unwrap_or_default();
    (out.status.code(), text[cut..].to_owned())
}

#[async_trait]
impl HostLink for LocalHost {
    async fn start_session(&self, _: HostId, _: &str, spec: SessionSpec) -> Result<SessionHandle> {
        (self.agent)(&self.worktree(), &spec.envelope);
        self.envelopes.lock().unwrap().push(spec.envelope);
        self.modes.lock().unwrap().push(spec.permission_mode);
        let (id, sink) = (spec.session_id, self.sink.clone());
        let (hold, stopped) = (self.hold.clone(), self.stopped.clone());
        tokio::spawn(async move {
            // Real output first: the watchdog fails an empty done claim.
            let script = [
                SunEvent::Text {
                    role: sun_core::Role::Assistant,
                    text: "worked in the worktree".into(),
                },
                SunEvent::DoneClaimed {
                    summary: "done".into(),
                },
                SunEvent::Exited { code: Some(0) },
            ];
            for (seq, event) in (1..).zip(script) {
                tokio::time::sleep(Duration::from_millis(5)).await;
                if seq > 1 && hold.load(Ordering::SeqCst) {
                    while hold.load(Ordering::SeqCst) {
                        tokio::time::sleep(Duration::from_millis(5)).await;
                    }
                    if stopped.lock().unwrap().contains(&id) {
                        return;
                    }
                }
                if let Some(sink) = sink.lock().unwrap().as_ref() {
                    let _ = sink.send(HostEvent {
                        session_id: id,
                        seq,
                        event,
                    });
                }
            }
        });
        Ok(SessionHandle {
            session_id: id,
            native_id: None,
            pid: None,
        })
    }

    async fn steer(&self, _: &SessionHandle, _: Steer) -> Result<()> {
        Ok(())
    }

    async fn stop_session(&self, handle: &SessionHandle, _: StopMode) -> Result<()> {
        self.stopped.lock().unwrap().insert(handle.session_id);
        Ok(())
    }

    async fn is_alive(&self, _: &SessionHandle) -> Result<bool> {
        Ok(false)
    }

    fn events(&self) -> BoxStream<'static, HostEvent> {
        let (tx, rx) = mpsc::unbounded_channel();
        *self.sink.lock().unwrap() = Some(tx);
        Box::pin(Receiver(rx))
    }

    async fn ack(&self, _: SessionId, _: u64) -> Result<()> {
        Ok(())
    }

    async fn prepare_worktree(&self, _: HostId, worktree: Worktree) -> Result<PathBuf> {
        // One worktree for every task (a memory PR's `Repo` one too).
        let _ = worktree;
        Ok(self.worktree())
    }

    /// Every page renders the same gradient: pixel `(x, y)` is `[x, y, 0, 255]`
    /// (so a crop shows where it was cut).
    async fn preview_screenshot(
        &self,
        _: HostId,
        request: sun_core::host_protocol::ScreenshotRequest,
    ) -> Result<Vec<u8>> {
        let paint = |x: u32, y: u32| [x as u8, y as u8, 0, 255];
        Ok(crate::screenshot::test_png(
            request.width,
            request.height,
            paint,
        ))
    }

    async fn exec(&self, _: HostId, request: ExecRequest) -> Result<ExecOutput> {
        assert_eq!(request.cwd, self.worktree());
        let (code, output) = sh_env(&request.cwd, &request.command, &request.env);
        Ok(ExecOutput {
            code,
            output,
            timed_out: false,
        })
    }

    async fn remove_worktree(&self, _: HostId, task_id: TaskId) -> Result<()> {
        self.removed.lock().unwrap().push(task_id);
        Ok(())
    }
}

pub(crate) struct Receiver<T>(pub(crate) mpsc::UnboundedReceiver<T>);

impl<T> futures_core::Stream for Receiver<T> {
    type Item = T;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<T>> {
        self.0.poll_recv(cx)
    }
}

/// Answers every call with `reply` and records it.
pub(crate) struct Mods {
    reply: Value,
    pub(crate) calls: Mutex<Vec<ModCall>>,
}

#[async_trait]
impl ModLink for Mods {
    async fn call(&self, call: ModCall) -> Result<Value> {
        self.calls.lock().unwrap().push(call);
        Ok(self.reply.clone())
    }
}

pub(crate) fn mods(reply: Value) -> Arc<Mods> {
    Arc::new(Mods {
        reply,
        calls: Mutex::default(),
    })
}

// ---------------------------------------------------------------- helpers

async fn start(
    store: &Store,
    host: Arc<LocalHost>,
    mods: Arc<Mods>,
) -> (Engine, tokio::task::JoinHandle<Result<()>>) {
    let config = EngineConfig {
        allow_bypass: false,
        host: HostId::generate(),
        adapter: "fake".into(),
        delivery_timeout: Duration::from_secs(5),
        watchdog: crate::WatchdogConfig::default(),
    };
    Engine::start(store.clone(), host, mods, config)
        .await
        .unwrap()
}

/// Saves `yaml` and one queued run of it.
pub(crate) async fn seed(store: &Store, yaml: &str) -> RunId {
    seed_with(store, yaml, vec![]).await
}

/// A repo row on Main at `url`, not yet inserted.
pub(crate) fn repo(url: &str) -> sun_core::model::Repo {
    sun_core::model::Repo {
        id: sun_core::RepoId::generate(),
        workspace_id: sun_core::WorkspaceId::MAIN,
        url: url.into(),
        default_branch: "main".into(),
        gate_config: json!({}),
        description: String::new(),
        hook_id: None,
    }
}

/// The task's branch (`sun/<KEY>-<slug>`), as the engine cuts it.
pub(crate) async fn branch_of(store: &Store, task_id: TaskId) -> String {
    let mut tx = store.read().await.unwrap();
    let task = tx.get_task(task_id).await.unwrap();
    crate::executor::tasks::branch_of(&mut tx, &task)
        .await
        .unwrap()
}

/// [`seed`] for a run in `repo_ids`.
pub(crate) async fn seed_with(store: &Store, yaml: &str, repo_ids: Vec<sun_core::RepoId>) -> RunId {
    let parsed = sun_workflow::parse(yaml).unwrap();
    let workflow = WorkflowRow {
        id: WorkflowId::generate(),
        workspace_id: sun_core::WorkspaceId::MAIN,
        name: parsed.name.clone(),
    };
    let task = Task {
        workflow_id: Some(workflow.id),
        status: TaskStatus::Todo,
        ..Task::new(sun_core::WorkspaceId::MAIN, "t")
    };
    let run = Run {
        id: RunId::generate(),
        task_id: task.id,
        workflow_id: workflow.id,
        workflow_version: 1,
        status: RunStatus::Queued,
        end_reason: None,
        budget: Budget::default(),
        repo_ids,
    };
    let mut tx = store.begin().await.unwrap();
    tx.insert_workflow(&workflow).await.unwrap();
    tx.save_workflow_version(workflow.id, yaml, &parsed.hash)
        .await
        .unwrap();
    tx.insert_task(&task).await.unwrap();
    tx.insert_run(&run).await.unwrap();
    tx.commit().await.unwrap();
    run.id
}

async fn run_of(store: &Store, id: RunId) -> Run {
    store.read().await.unwrap().get_run(id).await.unwrap()
}

pub(crate) async fn steps(store: &Store, id: RunId) -> Vec<Step> {
    store.read().await.unwrap().steps_of_run(id).await.unwrap()
}

async fn status_of(store: &Store, id: RunId, key: &str) -> StepStatus {
    crate::state::latest(steps(store, id).await)[&StepKey::new(key).unwrap()].status
}

pub(crate) async fn events(store: &Store, id: RunId, kind: &str) -> Vec<Value> {
    let page = store
        .read()
        .await
        .unwrap()
        .events_after(Some(id), 0, 10_000)
        .await
        .unwrap();
    page.into_iter()
        .filter(|e| e.kind == kind)
        .map(|e| e.payload)
        .collect()
}

pub(crate) async fn until_run_ends(store: &Store, id: RunId) -> Run {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let run = run_of(store, id).await;
        if !matches!(run.status, RunStatus::Queued | RunStatus::Running) {
            return run;
        }
        assert!(Instant::now() < deadline, "run still {}", run.status);
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

async fn until_step(store: &Store, id: RunId, key: &str, status: StepStatus) {
    let deadline = Instant::now() + Duration::from_secs(20);
    let key_of = StepKey::new(key).unwrap();
    // A step has no row until it is ready.
    while crate::state::latest(steps(store, id).await)
        .get(&key_of)
        .map(|s| s.status)
        != Some(status)
    {
        assert!(Instant::now() < deadline, "`{key}` never {status}");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

fn attempts(rows: &[Step], key: &str) -> Vec<(u32, StepStatus)> {
    rows.iter()
        .filter(|s| s.step_key.as_str() == key)
        .map(|s| (s.attempt, s.status))
        .collect()
}

const FIX: &str = r#"
name: fix
steps:
  - id: implement
    type: agent
    prompt: fix it
  - id: verify
    type: gate
    gates: [diff_not_empty, {test: {command: "test -f fixed.txt"}}]
    on_fail: { send_back_to: implement, max: MAX }
"#;

// ---------------------------------------------------------------- tests

#[tokio::test(flavor = "multi_thread")]
async fn gates_send_failure_output_back_to_the_agent_then_pass() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).await.unwrap();
    // The agent only fixes things once a gate told it what failed.
    let host = LocalHost::new(dir.path(), |ws, envelope| {
        if envelope.contains("Gate `diff_not_empty` failed") {
            std::fs::write(ws.join("fixed.txt"), "ok").unwrap();
        }
    });
    let (engine, _task) = start(&store, host.clone(), mods(json!({}))).await;
    let run = seed(&store, &FIX.replace("MAX", "2")).await;
    engine.start_run(run).await.unwrap();

    let ended = until_run_ends(&store, run).await;
    assert_eq!(ended.status, RunStatus::Succeeded);
    assert_eq!(ended.end_reason, None, "a test gate ran: verified");
    let rows = steps(&store, run).await;
    use StepStatus::Succeeded;
    assert_eq!(
        attempts(&rows, "implement"),
        [(1, Succeeded), (2, Succeeded)]
    );
    assert_eq!(attempts(&rows, "verify"), [(1, Succeeded)]);

    let envelopes = host.envelopes.lock().unwrap().clone();
    assert!(envelopes[0].contains("fix it"), "{}", envelopes[0]);
    assert!(
        envelopes[1].contains("fix it\n\nVerification step `verify` failed (send-back 1 of 2)")
    );
    assert!(
        envelopes[1].contains("no changes: the working tree is clean"),
        "{}",
        envelopes[1]
    );

    let results: Vec<(String, bool)> = events(&store, run, "gate_result")
        .await
        .iter()
        .map(|e| {
            (
                e["result"]["name"].as_str().unwrap().to_owned(),
                e["result"]["pass"] == true,
            )
        })
        .collect();
    // First pass stops at the first failure; the second runs both.
    let expected = [
        ("diff_not_empty", false),
        ("diff_not_empty", true),
        ("test", true),
    ];
    assert_eq!(results, expected.map(|(n, p)| (n.to_owned(), p)));
}

#[tokio::test(flavor = "multi_thread")]
async fn gates_give_up_after_max_send_backs() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).await.unwrap();
    let host = LocalHost::new(dir.path(), |_, _| {});
    let (engine, _task) = start(&store, host.clone(), mods(json!({}))).await;
    let run = seed(&store, &FIX.replace("MAX", "1")).await;
    engine.start_run(run).await.unwrap();

    let ended = until_run_ends(&store, run).await;
    assert_eq!(ended.status, RunStatus::Failed);
    assert_eq!(ended.end_reason.as_deref(), Some("step `verify` failed"));
    let rows = steps(&store, run).await;
    use StepStatus::{Failed, Succeeded};
    assert_eq!(
        attempts(&rows, "implement"),
        [(1, Succeeded), (2, Succeeded)]
    );
    assert_eq!(attempts(&rows, "verify"), [(1, Failed)]);
    assert_eq!(
        events(&store, run, ESCALATED).await,
        [json!({ "send_backs": 1, "reason": "gate failed: diff_not_empty, after 1 send-back(s)" })],
        "a person takes over"
    );
    let reasons: Vec<Value> = events(&store, run, "step_status")
        .await
        .into_iter()
        .filter(|e| e["to"] == "failed")
        .map(|e| e["reason"].clone())
        .collect();
    assert_eq!(
        reasons,
        [json!("gate failed: diff_not_empty, after 1 send-back(s)")]
    );
}

/// A person's retry of the escalated gate gives the agent a fresh set of
/// send-backs: counted per retry, not from the agent's attempt number.
#[tokio::test(flavor = "multi_thread")]
async fn send_backs_reset_on_a_manual_retry() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).await.unwrap();
    let host = LocalHost::new(dir.path(), |_, _| {});
    let (engine, _task) = start(&store, host.clone(), mods(json!({}))).await;
    let run = seed(&store, &FIX.replace("MAX", "1")).await;
    engine.start_run(run).await.unwrap();
    assert_eq!(until_run_ends(&store, run).await.status, RunStatus::Failed);

    let verify = StepKey::new("verify").unwrap();
    engine
        .retry_step(
            run,
            verify,
            crate::Actor {
                member_id: None,
                name: "user:ana".into(),
            },
        )
        .await
        .unwrap();
    for _ in 0..500 {
        if events(&store, run, ESCALATED).await.len() == 2 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(until_run_ends(&store, run).await.status, RunStatus::Failed);
    let rows = steps(&store, run).await;
    use StepStatus::{Failed, Succeeded};
    // One more send-back after the retry, then a person again.
    assert_eq!(
        attempts(&rows, "implement"),
        [(1, Succeeded), (2, Succeeded), (3, Succeeded)]
    );
    assert_eq!(attempts(&rows, "verify"), [(1, Failed), (2, Failed)]);
    let escalated = events(&store, run, ESCALATED).await;
    assert_eq!(escalated.len(), 2, "{escalated:?}");
    assert_eq!(escalated[1]["send_backs"], 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn gates_check_content_on_the_host_and_ask_mods() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).await.unwrap();
    let host = LocalHost::new(dir.path(), |ws, _| {
        let post = "# Intro\nMeet Sun, the engine.\n\n## Next steps\nTry it.\n";
        std::fs::write(ws.join("post.md"), post).unwrap();
    });
    let mods = mods(json!({ "pass": true, "output": "PR #7 is open" }));
    let (engine, _task) = start(&store, host, mods.clone()).await;
    let yaml = r#"
name: post
steps:
  - id: draft
    type: agent
    prompt: write the post
  - id: check
    type: gate
    gates:
      - sections_present: {file: post.md, sections: [Intro, Next steps]}
      - terms: {file: post.md, banned: [cheap], required: [sun]}
      - format: {file: post.md}
      - length: {file: post.md, max_words: 50}
      - links_resolve: {file: post.md}
      - pr_exists
"#;
    let web = repo("https://github.com/acme/web.git");
    let mut tx = store.begin().await.unwrap();
    tx.insert_repo(&web).await.unwrap();
    tx.commit().await.unwrap();
    let run = seed_with(&store, yaml, vec![web.id]).await;
    engine.start_run(run).await.unwrap();

    let ended = until_run_ends(&store, run).await;
    let results = events(&store, run, "gate_result").await;
    assert_eq!(ended.status, RunStatus::Succeeded, "{results:#?}");
    assert_eq!(results.len(), 6);
    assert_eq!(results[3]["result"]["output"], "11 words, 53 chars");
    assert_eq!(results[5]["result"]["output"], "PR #7 is open");
    let calls = mods.calls.lock().unwrap().clone();
    assert_eq!(
        (calls[0].mod_name.as_str(), calls[0].method.as_str()),
        ("github", "pr_exists")
    );
    let task_id = run_of(&store, run).await.task_id;
    assert_eq!(calls[0].params["branch"], branch_of(&store, task_id).await);
    // Five content gates read the same unchanged file: one artifact version.
    let artifacts = engine.artifacts(task_id).await.unwrap();
    assert_eq!(artifacts.len(), 1, "{artifacts:#?}");
    assert_eq!(
        (artifacts[0].name.as_str(), artifacts[0].mime.as_str()),
        ("post.md", "text/markdown")
    );
    let (_, bytes) = engine.artifact(artifacts[0].id).await.unwrap();
    assert!(String::from_utf8(bytes).unwrap().starts_with("# Intro"));
    assert_eq!(
        calls[0].idem_key, None,
        "a gate is a read, not a side effect"
    );
    // pr_exists is a code gate; without `test`/`ci_green` "done" is weak.
    assert_eq!(
        ended.end_reason.as_deref(),
        Some("done (unverified): no test or ci_green gate")
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn gates_with_bad_config_fail_without_send_back() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).await.unwrap();
    let host = LocalHost::new(dir.path(), |_, _| {});
    let (engine, _task) = start(&store, host.clone(), mods(json!({}))).await;
    let yaml = FIX.replace(
        r#"{test: {command: "test -f fixed.txt"}}"#,
        "{terms: {file: ../x}}",
    );
    let run = seed(&store, &yaml.replace("MAX", "2")).await;
    engine.start_run(run).await.unwrap();

    assert_eq!(until_run_ends(&store, run).await.status, RunStatus::Failed);
    assert_eq!(attempts(&steps(&store, run).await, "implement").len(), 1);
    assert!(events(&store, run, "gate_result").await.is_empty());
}

const REVIEW: &str = "
name: review
steps:
  - id: review
    type: approval
    who: [role:admin, user:carol]
";

#[tokio::test(flavor = "multi_thread")]
async fn gates_approval_moves_only_by_an_authorized_decision() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).await.unwrap();
    let host = LocalHost::new(dir.path(), |_, _| {});
    let (engine, task) = start(&store, host.clone(), mods(json!({}))).await;
    let run = seed(&store, REVIEW).await;
    engine.start_run(run).await.unwrap();
    until_step(&store, run, "review", StepStatus::WaitingApproval).await;
    let review = StepKey::new("review").unwrap();
    let bob = Principal {
        user: "bob".into(),
        roles: vec!["dev".into()],
    };

    // Not listed in `who:`.
    assert!(
        engine
            .decide(run, review.clone(), true, bob.clone())
            .await
            .is_err()
    );
    // Retries and agent claims can't move it.
    assert!(engine.start_run(run).await.is_err());
    assert!(
        engine
            .done(run, review.clone(), "approve me".into())
            .await
            .is_err()
    );
    // Neither can restarts: resume leaves it waiting.
    task.abort();
    let _ = task.await;
    for _ in 0..3 {
        let (again, handle) = start(&store, host.clone(), mods(json!({}))).await;
        assert!(again.start_run(run).await.is_err());
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(
            status_of(&store, run, "review").await,
            StepStatus::WaitingApproval
        );
        handle.abort();
        let _ = handle.await;
    }
    let (engine, _task) = start(&store, host, mods(json!({}))).await;
    assert_eq!(run_of(&store, run).await.status, RunStatus::Running);
    assert_eq!(attempts(&steps(&store, run).await, "review").len(), 1);

    engine
        .decide(
            run,
            review,
            true,
            Principal {
                roles: vec!["admin".into()],
                ..bob
            },
        )
        .await
        .unwrap();
    let ended = until_run_ends(&store, run).await;
    assert_eq!(ended.status, RunStatus::Succeeded);
    assert_eq!(ended.end_reason, None, "an approval verifies");
    let decisions = events(&store, run, "approval").await;
    assert_eq!(
        decisions,
        [json!({"approved": true, "by": "bob", "as": "role:admin", "reason": null})]
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn gates_approval_timeouts_remind_and_auto_reject() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).await.unwrap();
    let host = LocalHost::new(dir.path(), |_, _| {});
    let (engine, _task) = start(&store, host, mods(json!({}))).await;
    let yaml = "
name: slow
steps:
  - id: nudge
    type: approval
    who: [role:admin]
    timeout: { after: 1s, then: remind }
  - id: deadline
    type: approval
    who: [role:admin]
    timeout: { after: 3s, then: auto_reject }
    needs: []
";
    let run = seed(&store, yaml).await;
    engine.start_run(run).await.unwrap();

    let ended = until_run_ends(&store, run).await;
    assert_eq!(ended.status, RunStatus::Failed);
    assert_eq!(
        ended.end_reason.as_deref(),
        Some("step `deadline` rejected")
    );
    assert!(events(&store, run, "approval_reminder").await.len() >= 2);
    assert_eq!(status_of(&store, run, "nudge").await, StepStatus::Cancelled);
    let decisions = events(&store, run, "approval").await;
    assert_eq!(decisions, [json!({"approved": false, "by": "timeout"})]);
}

#[test]
fn gates_resolve_rejects_bad_config() {
    let spec = |name: &str, params: Option<Value>| GateSpec {
        name: name.into(),
        params,
    };
    let job = |specs: &[GateSpec]| {
        Job::new(
            HostId::generate(),
            Worktree::Folder {
                task_id: TaskId::generate(),
            },
            RunId::generate(),
            specs,
        )
    };
    assert!(job(&[spec("test", Some(json!({"cmd": "x"})))]).is_err());
    assert!(job(&[spec("lint", Some(json!({"command": " "})))]).is_err());
    assert!(job(&[spec("length", None)]).is_err());
    let ok = job(&[
        spec("test", None),
        spec("ci_green", Some(json!({"mod": "gitlab"}))),
        spec("acme.license_scan", None),
    ])
    .unwrap();
    assert!(
        matches!(&ok.gates[0].1, Gate::Exec { command, timeout_s: 600, .. } if command.contains("cargo test"))
    );
    assert!(
        matches!(&ok.gates[1].1, Gate::Mod { mod_name, method, .. } if mod_name == "gitlab" && method == "ci_green")
    );
    assert!(
        matches!(&ok.gates[2].1, Gate::Mod { mod_name, method, .. } if mod_name == "acme" && method == "license_scan")
    );
}

#[test]
fn gates_unverified_when_nothing_checks_the_work() {
    let verdict = |steps: &str| {
        let workflow = sun_workflow::parse(&format!("name: x\nsteps:\n{steps}")).unwrap();
        unverified(&workflow)
    };
    let agent = "  - id: a\n    type: agent\n    prompt: p\n";
    let none = "done (unverified): no gate or approval configured";
    assert_eq!(verdict(agent), Some(none));
    let approval = "  - id: r\n    type: approval\n    who: [role:admin]\n";
    assert_eq!(verdict(&format!("{agent}{approval}")), None);
    let gate = |gates: &str| format!("{agent}  - id: g\n    type: gate\n    gates: [{gates}]\n");
    let weak = "done (unverified): no test or ci_green gate";
    assert_eq!(verdict(&gate("diff_not_empty, lint")), Some(weak));
    assert_eq!(verdict(&gate("diff_not_empty, test")), None);
    // Content gates verify non-code work.
    assert_eq!(verdict(&gate("{terms: {file: a.md}}")), None);
}

/// A run that ends `done (unverified)` in a repo with a real npm test
/// script gets `npm test` suggested; npm's stub doesn't count.
#[tokio::test(flavor = "multi_thread")]
async fn unverified_task_suggests_npm_test() {
    use sun_store::RepoRepo;

    const UNGATED: &str = "name: quick\nsteps:\n  - id: a\n    type: agent\n    prompt: p\n";
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).await.unwrap();
    let host = LocalHost::new(dir.path(), |_, _| {});
    let (engine, _task) = start(&store, host.clone(), mods(json!({}))).await;
    let repo = repo("https://github.com/acme/web.git");
    let mut tx = store.begin().await.unwrap();
    tx.insert_repo(&repo).await.unwrap();
    tx.commit().await.unwrap();
    let suggested = |run: RunId, wait: Duration| {
        let store = store.clone();
        async move {
            let deadline = Instant::now() + wait;
            loop {
                let found = events(&store, run, GATE_SUGGESTED).await;
                if !found.is_empty() || Instant::now() > deadline {
                    break found;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        }
    };
    let package = |test: &str| {
        let json = json!({ "name": "web", "scripts": { "test": test } });
        std::fs::write(host.worktree().join("package.json"), json.to_string()).unwrap();
    };

    package("vitest run");
    let run = seed_with(&store, UNGATED, vec![repo.id]).await;
    engine.start_run(run).await.unwrap();
    let ended = until_run_ends(&store, run).await;
    assert_eq!(
        ended.end_reason.as_deref(),
        Some("done (unverified): no gate or approval configured")
    );
    assert_eq!(
        suggested(run, Duration::from_secs(10)).await,
        [json!({ "gate": "test", "command": "npm test" })]
    );

    package("echo \"Error: no test specified\" && exit 1");
    let stub = seed_with(&store, UNGATED, vec![repo.id]).await;
    engine.start_run(stub).await.unwrap();
    until_run_ends(&store, stub).await;
    assert!(
        suggested(stub, Duration::from_millis(500)).await.is_empty(),
        "npm's stub is not a test script"
    );
}

#[test]
fn gates_feedback_keeps_the_tail_of_long_output() {
    let long = GateResult {
        name: "test".into(),
        pass: false,
        output: format!("{}é{}", "x".repeat(FEEDBACK_TAIL), "END"),
        file: None,
        touched: Vec::new(),
        waiting: false,
    };
    let text = feedback(&StepKey::new("verify").unwrap(), 1, 2, &[&long]);
    assert!(text.contains("send-back 1 of 2") && text.contains("### Gate `test` failed"));
    assert!(text.contains("END\n```"));
    assert!(text.len() < FEEDBACK_TAIL + 300);
}

#[test]
fn gates_rerun_covers_the_path_back_to_the_target() {
    let wf = sun_workflow::parse(
        "
name: x
steps:
  - id: a
    type: agent
    prompt: p
  - id: b
    type: wait
    for: 1s
  - id: side
    type: wait
    for: 1s
    needs: [a]
  - id: g
    type: gate
    gates: [test]
    needs: [b]
",
    )
    .unwrap();
    let key = |k: &str| StepKey::new(k).unwrap();
    assert_eq!(rerun(&wf, &key("a"), &key("g")), [key("a"), key("b")]);
}

#[test]
fn gates_quote_shell_words() {
    assert_eq!(quote("it's"), r"'it'\''s'");
    let (code, out) = sh(Path::new("."), &format!("printf %s {}", quote("a'b $HOME")));
    assert_eq!((code, out.as_str()), (Some(0), "a'b $HOME"));
}

const DATA: &str = r#"
name: data
steps:
  - id: implement
    type: agent
    prompt: change things
  - id: verify
    type: gate
    gates: [{data_touched: {paths: ["infra/rds/**"]}}]
  - id: data_review
    type: approval
    when: data_touched
    who: [role:admin]
"#;

#[tokio::test(flavor = "multi_thread")]
async fn data_touched_gate_journals_files_and_requires_approval() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).await.unwrap();
    let host = LocalHost::new(dir.path(), |ws, _| {
        std::fs::create_dir_all(ws.join("db/migrations")).unwrap();
        std::fs::write(ws.join("db/migrations/001_users.sql"), "create table u();").unwrap();
        std::fs::write(ws.join(".env"), "DATABASE_URL=postgres://db/app").unwrap();
        std::fs::write(ws.join("notes.txt"), "hello").unwrap();
    });
    let (engine, _task) = start(&store, host, mods(json!({}))).await;
    let run = seed(&store, DATA).await;
    engine.start_run(run).await.unwrap();
    until_step(&store, run, "data_review", StepStatus::WaitingApproval).await;

    let detected = events(&store, run, "data_touched").await;
    assert_eq!(detected.len(), 1);
    assert_eq!(detected[0]["label"], "detected");
    assert_eq!(
        detected[0]["files"],
        json!([
            {"path": ".env", "rule": "data_config"},
            {"path": "db/migrations/001_users.sql", "rule": "migration"},
        ])
    );
    let gate = &events(&store, run, "gate_result").await[0]["result"];
    assert_eq!(gate["pass"], true);
    assert!(gate.get("touched").is_none(), "journalled as its own event");
    let task_id = run_of(&store, run).await.task_id;
    let badge = engine.data_touched(task_id).await.unwrap();
    assert_eq!(serde_json::to_value(badge).unwrap(), detected[0]["files"]);
    let admin = Principal {
        user: "ann".into(),
        roles: vec!["admin".into()],
    };
    let key = StepKey::new("data_review").unwrap();
    engine.decide(run, key, true, admin).await.unwrap();
    assert_eq!(
        until_run_ends(&store, run).await.status,
        RunStatus::Succeeded
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn data_touched_approval_is_skipped_when_nothing_was_touched() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).await.unwrap();
    let host = LocalHost::new(dir.path(), |ws, _| {
        std::fs::write(ws.join("README.md"), "docs only").unwrap();
    });
    let (engine, _task) = start(&store, host, mods(json!({}))).await;
    let run = seed(&store, DATA).await;
    engine.start_run(run).await.unwrap();

    assert_eq!(
        until_run_ends(&store, run).await.status,
        RunStatus::Succeeded
    );
    assert_eq!(
        events(&store, run, "data_touched").await[0]["files"],
        json!([])
    );
    let review = attempts(&steps(&store, run).await, "data_review");
    assert_eq!(review, [(1, StepStatus::Succeeded)]);
}

/// A task with no repo works in a folder; its outputs become
/// versioned artifacts and a delivery plugin's `upload` gets their bytes.
#[tokio::test(flavor = "multi_thread")]
async fn repo_less_output_reaches_gdrive() {
    use base64::Engine as _;
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).await.unwrap();
    let sessions = std::sync::atomic::AtomicUsize::new(0);
    let host = LocalHost::new(dir.path(), move |ws, _| {
        let first = sessions.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0;
        let text = if first {
            "# Q3 plan\nLong draft.\n"
        } else {
            "# Q3 plan\nShort.\n"
        };
        std::fs::write(ws.join("plan.md"), text).unwrap();
        std::fs::create_dir_all(ws.join("out")).unwrap();
        std::fs::write(ws.join("out/data.csv"), "a,b\n1,2\n").unwrap();
        std::fs::write(ws.join(".scratch"), "hidden").unwrap();
    });
    let mods = mods(json!({ "id": "drive-file-1", "url": "https://drive.google.com/file/d/1" }));
    let (engine, _task) = start(&store, host, mods.clone()).await;
    let yaml = r#"
name: plan
steps:
  - id: draft
    type: agent
    prompt: draft the plan
  - id: edit
    type: agent
    prompt: tighten the plan
  - id: deliver
    type: action
    use: gdrive.upload
    with: { folder_id: team-folder, files: [plan.md] }
"#;
    let run = seed(&store, yaml).await;
    engine.start_run(run).await.unwrap();

    assert_eq!(
        until_run_ends(&store, run).await.status,
        RunStatus::Succeeded
    );
    let task_id = run_of(&store, run).await.task_id;
    let mut saved: Vec<(String, u32)> = engine
        .artifacts(task_id)
        .await
        .unwrap()
        .into_iter()
        .map(|a| (a.name, a.version))
        .collect();
    saved.sort();
    // Each step's output is a version; an unchanged file is not a new one.
    assert_eq!(
        saved,
        [
            ("out/data.csv".into(), 1),
            ("plan.md".into(), 2),
            ("plan.md".into(), 3)
        ]
    );
    let calls = mods.calls.lock().unwrap().clone();
    let upload = calls
        .iter()
        .find(|c| (c.mod_name.as_str(), c.method.as_str()) == ("gdrive", "upload"))
        .expect("gdrive.upload was called");
    assert!(upload.idem_key.is_some(), "a delivery is keyed");
    assert_eq!(upload.params["folder_id"], "team-folder");
    assert_eq!(upload.params["title"], "t");
    let files = upload.params["files"].as_array().unwrap();
    assert_eq!(files.len(), 1, "{files:#?}");
    assert_eq!(
        (&files[0]["name"], &files[0]["content_type"]),
        (&json!("plan.md"), &json!("text/markdown"))
    );
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(files[0]["content_base64"].as_str().unwrap())
        .unwrap();
    assert_eq!(bytes, b"# Q3 plan\nShort.\n");
}

/// A task-folder file far past one exec's 64 KiB output (a 1 MiB
/// render) is read in chunks and versioned byte for byte.
#[tokio::test(flavor = "multi_thread")]
async fn repo_less_large_output_is_versioned() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).await.unwrap();
    let render: Vec<u8> = (0..1024 * 1024u32)
        .map(|i| (i.wrapping_mul(2_654_435_761) >> 13) as u8)
        .collect();
    let written = render.clone();
    let host = LocalHost::new(dir.path(), move |ws, _| {
        std::fs::write(ws.join("render.png"), &written).unwrap();
    });
    let (engine, _task) = start(&store, host, mods(json!({}))).await;
    let yaml = "name: render\nsteps:\n  - id: draw\n    type: agent\n    prompt: draw it\n";
    let run = seed(&store, yaml).await;
    engine.start_run(run).await.unwrap();
    assert_eq!(
        until_run_ends(&store, run).await.status,
        RunStatus::Succeeded
    );
    let task_id = run_of(&store, run).await.task_id;
    // The last step's snapshot may land just after the run ends.
    let deadline = Instant::now() + Duration::from_secs(10);
    let saved = loop {
        let saved = engine.artifacts(task_id).await.unwrap();
        if !saved.is_empty() || Instant::now() > deadline {
            break saved;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    };
    assert_eq!(saved.len(), 1, "{saved:?}");
    let (artifact, bytes) = engine.artifact(saved[0].id).await.unwrap();
    assert_eq!(
        (artifact.name.as_str(), artifact.version),
        ("render.png", 1)
    );
    assert!(
        bytes == render,
        "1 MiB read back intact ({} bytes)",
        bytes.len()
    );
}
