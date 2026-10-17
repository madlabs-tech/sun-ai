//! Agents end to end: the real engine, store and queue over a host whose
//! agent sessions keep working until the test lets them finish. Covers the
//! triggers (create, Backlog, comments, reassignment), a comment storm, the
//! hidden system workflow, and the conversation on a task's thread (the
//! agent's reply as a comment, a comment resuming its session, @mentions
//! and their chats, direct chats).
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::HashSet;
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use serde_json::{Value, json};
use sun_core::model::{Assignee, Host, Priority, Run, Task, TaskKind};
use sun_core::{
    AgentId, BoxStream, EngineHandle, Error, HostEvent, HostId, HostLink, ModCall, ModLink, Result,
    Role, RunStatus, SessionHandle, SessionId, SessionSpec, Steer, StepStatus, StopMode, SunEvent,
    TaskId, TaskStatus, WorkflowId, WorkspaceId,
};
use sun_engine::{
    Actor, AgentFields, Caps, Engine, EngineConfig, NewTask, Queue, QueueConfig, Slot,
    WatchdogConfig, WorkflowPick,
};
use sun_store::{
    HostRepo, RunRepo, RuntimeRepo, Store, TaskRepo, WorkflowRepo, WorkspaceItem, WorkspaceRepo,
};
use tokio::sync::mpsc;

/// Agent sessions that say they work, then wait until `finish` is set to
/// claim done. Every envelope and steer is kept.
#[derive(Default)]
struct HoldHost {
    finish: Arc<AtomicBool>,
    envelopes: Mutex<Vec<String>>,
    /// The adapter each session started with.
    adapters: Mutex<Vec<String>>,
    /// The skills each session got.
    skills: Mutex<Vec<Option<Vec<String>>>>,
    steers: Mutex<Vec<String>>,
    /// Sessions get a native id (`n-<session id>`), so they can be resumed.
    native: AtomicBool,
    /// A start that resumes a session is refused (the CLI lost it).
    refuse_resume: AtomicBool,
    /// Each started session's `(resume, native id)`.
    natives: Mutex<Vec<(Option<String>, Option<String>)>>,
    /// The done claims a finishing session sends, in order (default: one
    /// `done`).
    claims: Mutex<Vec<String>>,
    /// Sessions repeat one tool call until the watchdog stops them.
    looping: AtomicBool,
    /// Each host event is delivered twice (a host redelivering unacked).
    redeliver: AtomicBool,
    /// Hosts that are not connected now.
    offline: Mutex<HashSet<HostId>>,
    /// The host each session started on.
    started_on: Mutex<Vec<HostId>>,
    alive: Arc<Mutex<HashSet<SessionId>>>,
    sink: Arc<Mutex<Option<mpsc::UnboundedSender<HostEvent>>>>,
}

impl HoldHost {
    fn envelopes(&self) -> Vec<String> {
        self.envelopes.lock().unwrap().clone()
    }
}

#[async_trait]
impl HostLink for HoldHost {
    async fn prepare_worktree(
        &self,
        _: HostId,
        _: sun_core::host_protocol::Worktree,
    ) -> Result<PathBuf> {
        Ok(std::env::temp_dir())
    }

    async fn start_session(
        &self,
        host: HostId,
        adapter: &str,
        spec: SessionSpec,
    ) -> Result<SessionHandle> {
        let id = spec.session_id;
        if spec.resume.is_some() && self.refuse_resume.load(Ordering::SeqCst) {
            return Err(Error::InvalidInput("no conversation found".into()));
        }
        let native_id = match self.native.load(Ordering::SeqCst) {
            true => Some(spec.resume.clone().unwrap_or_else(|| format!("n-{id}"))),
            false => None,
        };
        let started = (spec.resume.clone(), native_id.clone());
        self.natives.lock().unwrap().push(started);
        self.started_on.lock().unwrap().push(host);
        self.adapters.lock().unwrap().push(adapter.to_owned());
        self.skills.lock().unwrap().push(spec.skills.clone());
        self.envelopes.lock().unwrap().push(spec.envelope);
        self.alive.lock().unwrap().insert(id);
        let (finish, alive, sink) = (self.finish.clone(), self.alive.clone(), self.sink.clone());
        let mut claims = self.claims.lock().unwrap().clone();
        if claims.is_empty() {
            claims.push("done".into());
        }
        let looping = self.looping.load(Ordering::SeqCst);
        let copies = if self.redeliver.load(Ordering::SeqCst) {
            2
        } else {
            1
        };
        tokio::spawn(async move {
            let send = |seq, event: SunEvent| {
                if let Some(sink) = sink.lock().unwrap().as_ref() {
                    for _ in 0..copies {
                        let _ = sink.send(HostEvent {
                            session_id: id,
                            seq,
                            event: event.clone(),
                        });
                    }
                }
            };
            tokio::time::sleep(Duration::from_millis(5)).await;
            let working = SunEvent::Text {
                role: Role::Assistant,
                text: "working".into(),
            };
            send(1, working);
            if looping {
                for seq in 2..5 {
                    let call = SunEvent::ToolCall {
                        name: "Bash".into(),
                        args_hash: 7,
                        summary: "cargo test".into(),
                    };
                    send(seq, call);
                }
            }
            while looping || !finish.load(Ordering::SeqCst) {
                if !alive.lock().unwrap().contains(&id) {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            if !alive.lock().unwrap().remove(&id) {
                return;
            }
            let mut seq = 1;
            for summary in claims {
                seq += 1;
                send(seq, SunEvent::DoneClaimed { summary });
            }
            send(seq + 1, SunEvent::Exited { code: Some(0) });
        });
        Ok(SessionHandle {
            session_id: id,
            native_id,
            pid: None,
        })
    }

    async fn steer(&self, _: &SessionHandle, msg: Steer) -> Result<()> {
        self.steers.lock().unwrap().push(msg.text);
        Ok(())
    }

    async fn stop_session(&self, session: &SessionHandle, _: StopMode) -> Result<()> {
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

    async fn ack(&self, _: SessionId, _: u64) -> Result<()> {
        Ok(())
    }

    fn is_online(&self, host: HostId) -> bool {
        !self.offline.lock().unwrap().contains(&host)
    }
}

struct Receiver<T>(mpsc::UnboundedReceiver<T>);

impl<T> futures_core::Stream for Receiver<T> {
    type Item = T;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<T>> {
        self.0.poll_recv(cx)
    }
}

struct NoMods;

#[async_trait]
impl ModLink for NoMods {
    async fn call(&self, _: ModCall) -> Result<Value> {
        Ok(json!({}))
    }
}

struct Kernel {
    engine: Engine,
    queue: Queue,
    store: Store,
    host: Arc<HoldHost>,
    /// The first enrolled host, attached to Main.
    host_id: HostId,
    /// Every enrolled host, in [`kernel_on`]'s order.
    hosts: Vec<HostId>,
    _dir: tempfile::TempDir,
}

/// One host that reports no capabilities (so it takes anything).
async fn kernel() -> Kernel {
    kernel_on(&[("box", None)], sun_engine::Caps::default()).await
}

/// A kernel over `hosts` (name, and the capabilities it reports: `fake`
/// ready with these exec env kinds, or none), each attached to Main.
async fn kernel_on(hosts: &[(&str, Option<&[&str]>)], caps: sun_engine::Caps) -> Kernel {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).await.unwrap();
    let mut ids = Vec::new();
    let mut tx = store.begin().await.unwrap();
    for (name, envs) in hosts {
        let capabilities = match envs {
            None => json!({}),
            Some(envs) => json!({
                "os": "linux", "arch": "x86_64", "cpus": 4,
                "runtimes": [{"adapter": "fake", "readiness":
                    {"installed": true, "version": "1", "logged_in": true, "detail": null}}],
                "exec_envs": envs.iter().map(|kind| json!(
                    {"kind": kind, "ready": true, "version": "1", "detail": null}
                )).collect::<Vec<_>>(),
            }),
        };
        let row = Host {
            id: HostId::generate(),
            name: (*name).into(),
            capabilities,
            enrolled_by: None,
        };
        tx.insert_host(&row).await.unwrap();
        tx.link(WorkspaceId::MAIN, WorkspaceItem::Host(row.id))
            .await
            .unwrap();
        ids.push(row.id);
    }
    tx.commit().await.unwrap();
    let host_id = ids[0];
    let host = Arc::new(HoldHost::default());
    let config = EngineConfig {
        host: host_id,
        adapter: "fake".into(),
        delivery_timeout: Duration::from_secs(5),
        watchdog: WatchdogConfig::default(),
    };
    let (engine, _) = Engine::start(store.clone(), host.clone(), Arc::new(NoMods), config)
        .await
        .unwrap();
    let slot = Slot {
        runtime: "fake".into(),
        host: host_id,
    };
    let (queue, _) = Queue::start(store.clone(), engine.clone(), QueueConfig { slot, caps });
    Kernel {
        engine,
        queue,
        store,
        host,
        host_id,
        hosts: ids,
        _dir: dir,
    }
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

impl Kernel {
    async fn agent(&self, name: &str) -> AgentId {
        let fields = AgentFields {
            name: Some(name.into()),
            instructions: Some(format!("You are {name}.")),
            runtime_chain: Some(vec!["fake".into()]),
            ..AgentFields::default()
        };
        self.engine
            .create_agent(
                WorkspaceId::MAIN,
                fields,
                "user:me",
                &crate::support::unlimited(),
            )
            .await
            .unwrap()
            .id
    }

    async fn task(&self, agent: AgentId, status: TaskStatus) -> TaskId {
        let new = NewTask {
            workspace_id: Some(WorkspaceId::MAIN),
            title: "Fix the login".into(),
            body: "It 500s.".into(),
            workflow: WorkflowPick::Agent(agent),
            exec_env: None,
            priority: Priority::None,
            status,
            project_id: None,
            actor: None,
        };
        self.engine.create_task(new).await.unwrap().id
    }

    async fn get(&self, task: TaskId) -> Task {
        self.store
            .read()
            .await
            .unwrap()
            .get_task(task)
            .await
            .unwrap()
    }

    async fn runs(&self, task: TaskId) -> Vec<Run> {
        self.store
            .read()
            .await
            .unwrap()
            .runs_of_task(task)
            .await
            .unwrap()
    }

    async fn until(&self, task: TaskId, status: TaskStatus) {
        eventually(&format!("task {status}"), || async {
            self.get(task).await.status == status
        })
        .await;
    }

    /// Waits until the task's latest run has a session working on its step.
    async fn working(&self, task: TaskId) {
        eventually("a working session", || async {
            let Some(run) = self.runs(task).await.pop() else {
                return false;
            };
            let steps = self.store.read().await.unwrap().steps_of_run(run.id).await;
            let running = steps
                .unwrap()
                .iter()
                .any(|s| s.status == StepStatus::Running);
            let started = !self.host.alive.lock().unwrap().is_empty();
            running && started
        })
        .await;
    }

    async fn comment(&self, task: TaskId, text: &str) {
        self.engine.comment(task, text, None, &[]).await.unwrap();
    }

    /// The bodies of the task's comments, oldest first.
    async fn comments(&self, task: TaskId) -> Vec<Value> {
        let activity = self.engine.activity(task, 0).await.unwrap();
        activity
            .into_iter()
            .filter(|a| a.kind == sun_core::model::ActivityKind::Comment)
            .map(|a| a.body)
            .collect()
    }
}

fn ana() -> Option<Actor> {
    Some(Actor {
        member_id: None,
        name: "user:ana".into(),
    })
}

#[tokio::test(flavor = "multi_thread")]
async fn agents_start_on_todo_not_in_backlog_and_comments_steer_or_wake_them() {
    let k = kernel().await;
    let ada = k.agent("Ada").await;

    // Backlog: nothing runs until it leaves Backlog.
    let parked = k.task(ada, TaskStatus::Backlog).await;
    assert!(k.runs(parked).await.is_empty());
    let parked_task = k.get(parked).await;
    assert_eq!(parked_task.assignee(), Some(Assignee::Agent { id: ada }));

    // Todo on create: a run starts, on the agent's instructions.
    let task = k.task(ada, TaskStatus::Todo).await;
    k.until(task, TaskStatus::InProgress).await;
    k.working(task).await;
    assert!(k.host.envelopes()[0].contains("You are Ada."));

    // A comment while it runs steers it.
    k.comment(task, "Use the v2 API").await;
    assert_eq!(*k.host.steers.lock().unwrap(), ["Use the v2 API"]);
    assert_eq!(k.runs(task).await.len(), 1);

    // It finishes: Needs you (no approval step). A comment wakes it: a new
    // run, the comment in its first prompt, the task back to Todo first.
    k.host.finish.store(true, Ordering::SeqCst);
    k.until(task, TaskStatus::NeedsYou).await;
    k.host.finish.store(false, Ordering::SeqCst);
    k.comment(task, "Also fix signup").await;
    eventually("a second run", || async { k.runs(task).await.len() == 2 }).await;
    k.until(task, TaskStatus::InProgress).await;
    eventually("its session", || async { k.host.envelopes().len() == 2 }).await;
    assert!(k.host.envelopes()[1].contains("Also fix signup"));

    // Stop: Blocked. A comment resumes it with the comment in context.
    k.engine.interrupt(task, None).await.unwrap();
    k.until(task, TaskStatus::Blocked).await;
    k.comment(task, "Go on, skip the tests").await;
    eventually("a resumed session", || async {
        k.host
            .envelopes()
            .last()
            .is_some_and(|e| e.contains("Go on, skip the tests"))
    })
    .await;
    k.until(task, TaskStatus::InProgress).await;

    // Moving the parked one out of Backlog starts it.
    k.engine
        .move_task_manually(parked, TaskStatus::Todo, None)
        .await
        .unwrap();
    eventually("its run", || async { k.runs(parked).await.len() == 1 }).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_comment_storm_on_a_stopped_agent_task_queues_one_run() {
    let k = kernel().await;
    let ada = k.agent("Ada").await;
    k.host.finish.store(true, Ordering::SeqCst);
    let task = k.task(ada, TaskStatus::Todo).await;
    k.until(task, TaskStatus::NeedsYou).await;
    k.host.finish.store(false, Ordering::SeqCst);
    let comments = (0..8).map(|i| {
        let engine = k.engine.clone();
        tokio::spawn(async move { engine.comment(task, &format!("more {i}"), None, &[]).await })
    });
    for comment in comments.collect::<Vec<_>>() {
        comment.await.unwrap().unwrap();
    }
    let runs = k.runs(task).await;
    assert_eq!(runs.len(), 2, "one new run for the storm: {runs:?}");
    // Every comment reaches that run's agent: the first session or a steer.
    k.until(task, TaskStatus::InProgress).await;
    eventually("every comment delivered", || async {
        let envelope = k.host.envelopes().last().cloned().unwrap_or_default();
        let steers = k.host.steers.lock().unwrap().clone();
        (0..8).all(|i| {
            let text = format!("more {i}");
            envelope.contains(&text) || steers.contains(&text)
        })
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn reassigning_cancels_the_run_and_queues_one_for_the_new_agent() {
    let k = kernel().await;
    let (ada, bob) = (k.agent("Ada").await, k.agent("Bob").await);
    let task = k.task(ada, TaskStatus::Todo).await;
    k.until(task, TaskStatus::InProgress).await;
    let to_bob = Some(Assignee::Agent { id: bob });
    k.engine.assign_task(task, to_bob, None).await.unwrap();
    let runs = k.runs(task).await;
    assert_eq!(runs.len(), 2);
    assert_eq!(runs[0].status, RunStatus::Cancelled);
    // The old run no longer moves the task: never Cancelled on the way.
    let reassigned = k.get(task).await;
    assert_eq!(reassigned.assignee_agent_id, Some(bob));
    assert_ne!(reassigned.status, TaskStatus::Cancelled);
    k.until(task, TaskStatus::InProgress).await;
    eventually("Bob's session", || async {
        k.host
            .envelopes()
            .last()
            .is_some_and(|e| e.contains("You are Bob."))
    })
    .await;
    assert_ne!(runs[0].workflow_id, runs[1].workflow_id);

    // Unassigned: the run is cancelled and it is a plain task.
    k.engine.assign_task(task, None, None).await.unwrap();
    let plain = k.get(task).await;
    assert_eq!((plain.workflow_id, plain.assignee()), (None, None));
    assert_eq!(k.runs(task).await[1].status, RunStatus::Cancelled);
    // A task that has run can't take a workflow.
    let (workflow, _) = k
        .engine
        .save_workflow(
            None,
            "name: ship\nsteps:\n  - id: a\n    type: approval\n    who: [role:admin]\n",
        )
        .await
        .unwrap();
    let pick = Some(Assignee::Workflow { id: workflow.id });
    let err = k.engine.assign_task(task, pick, None).await.unwrap_err();
    assert!(matches!(err, Error::InvalidInput(_)), "{err}");
    // An archived agent takes no new tasks.
    k.engine.archive_agent(ada, "user:me").await.unwrap();
    let to_ada = Some(Assignee::Agent { id: ada });
    let err = k.engine.assign_task(task, to_ada, None).await.unwrap_err();
    assert!(matches!(err, Error::InvalidInput(_)), "{err}");
}

#[tokio::test(flavor = "multi_thread")]
async fn system_workflows_are_hidden_and_follow_agent_edits() {
    let k = kernel().await;
    let ada = k.agent("Ada").await;
    let task = k.task(ada, TaskStatus::Backlog).await;
    let system: WorkflowId = k.get(task).await.workflow_id.unwrap();
    // Hidden from every list, the builder and the default-workflow select.
    assert!(k.engine.workflows().await.unwrap().is_empty());
    let on_main = k.engine.workflows_of_workspace(WorkspaceId::MAIN).await;
    assert!(on_main.unwrap().is_empty());
    assert!(matches!(
        k.engine.workflow(system).await,
        Err(Error::NotFound(_))
    ));
    let yaml = "name: mine\nsteps:\n  - id: a\n    type: approval\n    who: [role:admin]\n";
    assert!(matches!(
        k.engine.save_workflow(Some(system), yaml).await,
        Err(Error::NotFound(_))
    ));
    let default = sun_engine::ProfileChange {
        default_workflow_id: Some(Some(system)),
        ..Default::default()
    };
    let err = k
        .engine
        .update_workspace_profile(WorkspaceId::MAIN, &default, "user:me")
        .await
        .unwrap_err();
    assert!(matches!(err, Error::InvalidInput(_)), "{err}");
    let on_system = NewTask {
        workspace_id: None,
        title: "sneak".into(),
        body: String::new(),
        workflow: WorkflowPick::Id(system),
        exec_env: None,
        priority: Priority::None,
        status: TaskStatus::Backlog,
        project_id: None,
        actor: None,
    };
    assert!(matches!(
        k.engine.create_task(on_system).await,
        Err(Error::NotFound(_))
    ));
    // One per (agent, workspace): another task reuses it.
    let again = k.task(ada, TaskStatus::Backlog).await;
    assert_eq!(k.get(again).await.workflow_id, Some(system));

    // An edit saves a new version (renamed with the agent); runs keep theirs.
    let edit = AgentFields {
        name: Some("Ada Two".into()),
        instructions: Some("Write tests first.".into()),
        ..AgentFields::default()
    };
    k.engine.update_agent(ada, edit, "user:me").await.unwrap();
    let mut tx = k.store.read().await.unwrap();
    let latest = tx.latest_workflow_version(system).await.unwrap().unwrap();
    assert_eq!(latest.version, 2);
    assert!(latest.source_yaml.contains("Write tests first."));
    assert_eq!(tx.get_workflow(system).await.unwrap().name, "Ada Two");
    drop(tx);
    // Names stay unique among active agents, ignoring case.
    let twin = AgentFields {
        name: Some("ada two".into()),
        runtime_chain: Some(vec!["fake".into()]),
        ..AgentFields::default()
    };
    let err = k
        .engine
        .create_agent(
            WorkspaceId::MAIN,
            twin,
            "user:me",
            &crate::support::unlimited(),
        )
        .await
        .unwrap_err();
    assert!(matches!(err, Error::InvalidInput(_)), "{err}");
}

#[tokio::test(flavor = "multi_thread")]
async fn the_prompt_is_the_instructions_then_the_task_then_the_waking_comment() {
    let k = kernel().await;
    let ada = k.agent("Ada").await;
    let task = k.task(ada, TaskStatus::Todo).await;
    k.until(task, TaskStatus::InProgress).await;
    k.working(task).await;
    let first = k.host.envelopes()[0].clone();
    assert!(
        first.contains("You are Ada.\n\nYour task: MAIN-1 Fix the login\n\nIt 500s."),
        "{first}"
    );

    // Woken by a comment: the instructions and the task again, then the comment.
    k.host.finish.store(true, Ordering::SeqCst);
    k.until(task, TaskStatus::NeedsYou).await;
    k.host.finish.store(false, Ordering::SeqCst);
    k.comment(task, "Also fix signup").await;
    eventually("its session", || async { k.host.envelopes().len() == 2 }).await;
    let woken = k.host.envelopes()[1].clone();
    let at = |text: &str| {
        woken
            .find(text)
            .unwrap_or_else(|| panic!("{text}: {woken}"))
    };
    assert!(at("You are Ada.") < at("Your task: MAIN-1 Fix the login"));
    assert!(at("It 500s.") < at("Also fix signup"));

    // No instructions: the task alone.
    let fields = AgentFields {
        name: Some("Bob".into()),
        runtime_chain: Some(vec!["fake".into()]),
        ..AgentFields::default()
    };
    let bob = k
        .engine
        .create_agent(
            WorkspaceId::MAIN,
            fields,
            "user:me",
            &crate::support::unlimited(),
        )
        .await
        .unwrap()
        .id;
    let plain = k.task(bob, TaskStatus::Todo).await;
    k.until(plain, TaskStatus::InProgress).await;
    eventually("Bob's session", || async { k.host.envelopes().len() == 3 }).await;
    let bare = k.host.envelopes()[2].clone();
    assert!(
        bare.contains("Your task: MAIN-2 Fix the login\n\nIt 500s."),
        "{bare}"
    );
    assert!(!bare.contains("You are"), "{bare}");
    let system = k.get(plain).await.workflow_id.unwrap();
    let mut tx = k.store.read().await.unwrap();
    let source = tx.latest_workflow_version(system).await.unwrap().unwrap();
    assert!(
        source.source_yaml.contains("\"prompt\": \"\""),
        "{source:?}"
    );
    drop(tx);

    // Any real text is kept, even text that reads like a marker. With
    // repos on the workspace, the brief lists them and says how to work:
    // the session itself runs in an empty folder and clones.
    for (url, what) in [
        ("https://github.com/acme/web.git", "the web app"),
        ("git@github.com:acme/api.git", ""),
    ] {
        k.engine
            .add_workspace_repo(WorkspaceId::MAIN, url, None, what, "http://hook", "user:me")
            .await
            .unwrap();
    }
    let fields = AgentFields {
        name: Some("Cy".into()),
        instructions: Some("(no instructions)".into()),
        runtime_chain: Some(vec!["fake".into()]),
        ..AgentFields::default()
    };
    let cy = k
        .engine
        .create_agent(
            WorkspaceId::MAIN,
            fields,
            "user:me",
            &crate::support::unlimited(),
        )
        .await
        .unwrap()
        .id;
    let literal = k.task(cy, TaskStatus::Todo).await;
    k.until(literal, TaskStatus::InProgress).await;
    eventually("Cy's session", || async { k.host.envelopes().len() == 4 }).await;
    let kept = k.host.envelopes()[3].clone();
    assert!(
        kept.contains("(no instructions)\n\nYour task: MAIN-3 Fix the login"),
        "{kept}"
    );
    assert!(
        kept.contains(
            "## Repositories agents can use\n\n- https://github.com/acme/web.git — the web app\n\
             - git@github.com:acme/api.git\n\nClone what you need into this folder. Work on \
             branch `sun/MAIN-3-fix-the-login`, push with this machine's git credentials, and \
             open a pull request whose title starts with `MAIN-3`."
        ),
        "{kept}"
    );
    assert!(matches!(
        k.engine.worktree(literal).await.unwrap(),
        sun_core::host_protocol::Worktree::Folder { .. }
    ));
}

#[tokio::test(flavor = "multi_thread")]
async fn saving_an_agent_regenerates_a_stale_system_workflow_version() {
    let k = kernel().await;
    let ada = k.agent("Ada").await;
    let task = k.task(ada, TaskStatus::Backlog).await;
    let system = k.get(task).await.workflow_id.unwrap();
    // A version an older engine wrote (its no-instructions marker).
    let mut tx = k.store.begin().await.unwrap();
    let v1 = tx.latest_workflow_version(system).await.unwrap().unwrap();
    let stale = v1
        .source_yaml
        .replace("You are Ada.\\n", "(no instructions)\\n");
    assert_ne!(stale, v1.source_yaml);
    tx.save_workflow_version(system, &stale, &v1.schema_hash)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    // A save with nothing changed regenerates it, once.
    for _ in 0..2 {
        let same = AgentFields::default();
        k.engine.update_agent(ada, same, "user:me").await.unwrap();
    }
    let mut tx = k.store.read().await.unwrap();
    let latest = tx.latest_workflow_version(system).await.unwrap().unwrap();
    assert_eq!((latest.version, &latest.source_yaml), (3, &v1.source_yaml));
}

#[tokio::test(flavor = "multi_thread")]
async fn an_agents_final_message_is_its_comment_and_wakes_nobody() {
    let k = kernel().await;
    let ada = k.agent("Ada").await;
    k.host.finish.store(true, Ordering::SeqCst);
    let task = k.task(ada, TaskStatus::Todo).await;
    k.until(task, TaskStatus::NeedsYou).await;
    let run = k.runs(task).await[0].id;
    let reply = json!({ "text": "done", "by_agent": ada, "run_id": run });
    assert_eq!(k.comments(task).await, [reply]);
    // No member wrote it, and it wakes nobody: no new run.
    let activity = k.engine.activity(task, 0).await.unwrap();
    assert!(activity.iter().all(|a| a.actor_member_id.is_none()));
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(k.runs(task).await.len(), 1);
    assert_eq!(k.get(task).await.status, TaskStatus::NeedsYou);

    // `sun.done` claims through the engine: the same comment, once.
    k.host.finish.store(false, Ordering::SeqCst);
    k.comment(task, "Also fix signup").await;
    k.until(task, TaskStatus::InProgress).await;
    k.working(task).await;
    let run = k.runs(task).await[1].id;
    let step = sun_core::StepKey::new(sun_engine::AGENT_STEP).unwrap();
    k.engine
        .done(run, step, "Fixed signup too.".into())
        .await
        .unwrap();
    k.until(task, TaskStatus::NeedsYou).await;
    k.host.finish.store(true, Ordering::SeqCst);
    tokio::time::sleep(Duration::from_millis(200)).await;
    let texts: Vec<Value> = k
        .comments(task)
        .await
        .iter()
        .map(|c| c["text"].clone())
        .collect();
    assert_eq!(texts, ["done", "Also fix signup", "Fixed signup too."]);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_comment_resumes_the_agents_last_session_or_a_fresh_one_reads_the_thread() {
    let k = kernel().await;
    k.host.native.store(true, Ordering::SeqCst);
    let ada = k.agent("Ada").await;
    k.host.finish.store(true, Ordering::SeqCst);
    let task = k.task(ada, TaskStatus::Todo).await;
    k.until(task, TaskStatus::NeedsYou).await;

    // A comment: the next run resumes the last session with the new message.
    k.host.finish.store(false, Ordering::SeqCst);
    k.engine
        .comment(task, "Also fix signup", ana(), &[])
        .await
        .unwrap();
    eventually("the resumed session", || async {
        k.host.envelopes().len() == 2
    })
    .await;
    let natives = k.host.natives.lock().unwrap().clone();
    assert!(natives[0].1.is_some());
    assert_eq!(natives[1].0, natives[0].1, "resumes the first session");
    let resumed = k.host.envelopes()[1].clone();
    assert!(resumed.contains("Sun resumed this session"), "{resumed}");
    assert!(resumed.contains("- ana: Also fix signup"), "{resumed}");
    assert!(!resumed.contains("Your task:"), "{resumed}");

    // The CLI lost it: a fresh session with the task and the thread.
    k.host.finish.store(true, Ordering::SeqCst);
    k.until(task, TaskStatus::NeedsYou).await;
    k.host.finish.store(false, Ordering::SeqCst);
    k.host.refuse_resume.store(true, Ordering::SeqCst);
    k.engine
        .comment(task, "And the logout", ana(), &[])
        .await
        .unwrap();
    eventually("a fresh session", || async {
        k.host.envelopes().len() == 3
    })
    .await;
    let fresh = k.host.envelopes()[2].clone();
    let at = |text: &str| {
        fresh
            .find(text)
            .unwrap_or_else(|| panic!("{text}: {fresh}"))
    };
    assert!(at("You are Ada.") < at("Your task: MAIN-1 Fix the login"));
    let thread = at("The conversation on this task so far, oldest first:");
    assert!(thread < at("- Ada (you): done\n- ana: Also fix signup\n- Ada (you): done"));
    assert!(
        at("- ana: And the logout\n\nMessages left for this step:\n- ana: And the logout") > thread
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_mention_wakes_only_the_named_agents_and_their_replies_land_on_the_task() {
    let k = kernel().await;
    let web = "https://github.com/acme/web.git";
    k.engine
        .add_workspace_repo(WorkspaceId::MAIN, web, None, "", "http://hook", "user:me")
        .await
        .unwrap();
    let (ada, bob) = (k.agent("Ada").await, k.agent("Bob").await);
    let task = k.task(ada, TaskStatus::Todo).await;
    k.until(task, TaskStatus::InProgress).await;
    k.working(task).await;

    // Bob is mentioned: Ada isn't steered; Bob answers from his own chat.
    let ask = "@Bob is the v2 API stable?";
    k.engine
        .comment(task, ask, ana(), &[bob, bob])
        .await
        .unwrap();
    eventually("Bob's session", || async { k.host.envelopes().len() == 2 }).await;
    assert!(k.host.steers.lock().unwrap().is_empty());
    let asked = k.host.envelopes()[1].clone();
    let brief = "You are Bob.\n\nYou were mentioned on task MAIN-1 Fix the login.";
    assert!(asked.contains(brief), "{asked}");
    assert!(asked.contains("It 500s."), "{asked}");
    assert!(asked.contains(&format!("- ana: {ask}")), "{asked}");
    assert!(
        !asked.contains("Clone what you need"),
        "a chat clones nothing: {asked}"
    );
    assert_eq!(k.comments(task).await[0]["mentions"], json!([bob]));
    let mut tx = k.store.read().await.unwrap();
    let chat = tx.mention_chat(task, bob).await.unwrap().unwrap();
    drop(tx);
    assert_eq!(
        (chat.kind, chat.number, chat.source_task_id),
        (TaskKind::Chat, None, Some(task))
    );
    // Hidden: in no task list, no "tasks assigned" count.
    let listed: Vec<TaskId> = k
        .engine
        .tasks()
        .await
        .unwrap()
        .iter()
        .map(|t| t.id)
        .collect();
    assert_eq!(listed, [task]);
    let counts = k.engine.agent_task_counts(|_| true).await.unwrap();
    assert_eq!((counts.get(&ada), counts.get(&bob)), (Some(&1), None));

    // Both finish; Bob's reply is on the task, by Bob.
    k.host.finish.store(true, Ordering::SeqCst);
    k.until(task, TaskStatus::NeedsYou).await;
    eventually("Bob's reply", || async {
        let comments = k.comments(task).await;
        comments.iter().any(|c| c["by_agent"] == json!(bob))
    })
    .await;
    assert!(k.comments(chat.id).await.is_empty());

    // Bob again: the same chat, its next run; Ada sleeps on.
    k.host.finish.store(false, Ordering::SeqCst);
    let again = "@Bob and v3?";
    k.engine.comment(task, again, ana(), &[bob]).await.unwrap();
    eventually("Bob's second run", || async {
        k.runs(chat.id).await.len() == 2
    })
    .await;
    assert_eq!(k.runs(task).await.len(), 1);

    // Naming the assignee wakes her on the task itself.
    k.engine
        .comment(task, "@Ada over to you", ana(), &[ada])
        .await
        .unwrap();
    eventually("Ada's second run", || async {
        k.runs(task).await.len() == 2
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn mentions_name_up_to_five_active_agents_of_the_workspace_and_never_in_a_chat() {
    let k = kernel().await;
    let ada = k.agent("Ada").await;
    let task = k.task(ada, TaskStatus::Backlog).await;
    let refused =
        |r: Result<sun_core::model::TaskActivity>| matches!(r, Err(Error::InvalidInput(_)));
    let unknown = AgentId::generate();
    assert!(refused(
        k.engine.comment(task, "hi", ana(), &[unknown]).await
    ));
    let mut six = Vec::new();
    for i in 0..6 {
        six.push(k.agent(&format!("Helper {i}")).await);
    }
    assert!(refused(k.engine.comment(task, "all", ana(), &six).await));
    let gone = k.agent("Gone").await;
    k.engine.archive_agent(gone, "user:me").await.unwrap();
    assert!(refused(k.engine.comment(task, "hi", ana(), &[gone]).await));
    let policy = sun_core::model::SignupPolicy::default();
    let ops = k
        .engine
        .create_workspace("Ops", None, None, &policy, "user:me")
        .await
        .unwrap()
        .id;
    let fields = AgentFields {
        name: Some("Opsie".into()),
        runtime_chain: Some(vec!["fake".into()]),
        ..AgentFields::default()
    };
    let elsewhere = k
        .engine
        .create_agent(ops, fields, "user:me", &crate::support::unlimited())
        .await
        .unwrap()
        .id;
    assert!(refused(
        k.engine.comment(task, "hi", ana(), &[elsewhere]).await
    ));

    // A chat takes no mentions.
    let bob = six[0];
    k.engine
        .comment(task, "@Helper 0?", ana(), &[bob])
        .await
        .unwrap();
    let mut tx = k.store.read().await.unwrap();
    let chat = tx.mention_chat(task, bob).await.unwrap().unwrap();
    drop(tx);
    assert!(refused(
        k.engine.comment(chat.id, "@Ada", ana(), &[ada]).await
    ));
    // Only the accepted comment was written.
    assert_eq!(k.comments(task).await.len(), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_chat_answers_its_creator_and_says_what_it_waits_behind() {
    let one_at_a_time = Caps {
        per_ecosystem: Some(1),
        ..Caps::default()
    };
    let k = kernel_on(&[("box", None)], one_at_a_time).await;
    let ada = k.agent("Ada").await;
    let task = k.task(ada, TaskStatus::Todo).await;
    k.until(task, TaskStatus::InProgress).await;
    k.working(task).await;

    let chat = k.engine.create_chat(ada, None).await.unwrap();
    assert_eq!(
        (chat.kind, chat.number, chat.status, chat.source_task_id),
        (TaskKind::Chat, None, TaskStatus::NeedsYou, None)
    );
    k.engine
        .comment(chat.id, "What are you on?", ana(), &[])
        .await
        .unwrap();
    // The only slot is Ada's task: the chat waits behind it, and says so.
    let chats = k.engine.chats(WorkspaceId::MAIN, None).await.unwrap();
    assert_eq!(chats.len(), 1);
    assert_eq!(chats[0].run.as_ref().unwrap().status, RunStatus::Queued);
    let behind: Vec<String> = chats[0]
        .queued_behind
        .iter()
        .map(ToString::to_string)
        .collect();
    assert_eq!(behind, ["MAIN-1"]);
    assert_eq!(
        chats[0].last.as_ref().unwrap().body["text"],
        "What are you on?"
    );

    // The task ends; the chat's session gets the chat brief and answers in the chat.
    k.host.finish.store(true, Ordering::SeqCst);
    eventually("the chat's session", || async {
        k.host.envelopes().len() == 2
    })
    .await;
    let asked = k.host.envelopes()[1].clone();
    assert!(
        asked.contains("You are Ada.\n\nYou are chatting with a person."),
        "{asked}"
    );
    assert!(asked.contains("- ana: What are you on?"), "{asked}");
    assert!(!asked.contains("Your task:"), "{asked}");
    eventually("Ada's reply in the chat", || async {
        let comments = k.comments(chat.id).await;
        comments.iter().any(|c| c["by_agent"] == json!(ada))
    })
    .await;
    // Someone else's list doesn't have it; no task list does.
    let theirs = k
        .engine
        .chats(WorkspaceId::MAIN, Some(sun_core::MemberId::generate()));
    assert!(theirs.await.unwrap().is_empty());
    let listed: Vec<TaskId> = k
        .engine
        .tasks()
        .await
        .unwrap()
        .iter()
        .map(|t| t.id)
        .collect();
    assert_eq!(listed, [task]);
}

/// Runs of `task` still going (queued, running or paused).
async fn live_runs(k: &Kernel, task: TaskId) -> usize {
    use sun_core::StateMachine;
    k.runs(task)
        .await
        .iter()
        .filter(|r| !r.status.is_terminal())
        .count()
}

/// Eight comments on `task` at once.
async fn storm(k: &Kernel, task: TaskId, what: &str) {
    let comments: Vec<_> = (0..8)
        .map(|i| {
            let engine = k.engine.clone();
            let text = format!("{what} {i}");
            tokio::spawn(async move { engine.comment(task, &text, None, &[]).await })
        })
        .collect();
    for comment in comments {
        comment.await.unwrap().unwrap();
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn comment_storms_on_failed_or_blocked_agent_tasks_keep_one_live_run() {
    let k = kernel().await;
    let ada = k.agent("Ada").await;

    // Failed: the storm makes one new run.
    let failed = k.task(ada, TaskStatus::Todo).await;
    k.working(failed).await;
    k.engine
        .move_task_manually(failed, TaskStatus::Failed, None)
        .await
        .unwrap();
    k.until(failed, TaskStatus::Failed).await;
    assert_eq!(live_runs(&k, failed).await, 0);
    let before = k.runs(failed).await.len();
    storm(&k, failed, "failed").await;
    assert_eq!(k.runs(failed).await.len(), before + 1);
    assert_eq!(live_runs(&k, failed).await, 1);
    k.until(failed, TaskStatus::InProgress).await;

    // Blocked (Stop): the storm resumes it or starts one run, never two.
    let blocked = k.task(ada, TaskStatus::Todo).await;
    k.working(blocked).await;
    k.engine.interrupt(blocked, None).await.unwrap();
    k.until(blocked, TaskStatus::Blocked).await;
    let before = k.runs(blocked).await.len();
    storm(&k, blocked, "blocked").await;
    assert!(k.runs(blocked).await.len() <= before + 1);
    assert_eq!(live_runs(&k, blocked).await, 1);
    k.until(blocked, TaskStatus::InProgress).await;
    assert_eq!(live_runs(&k, blocked).await, 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_comment_storm_racing_a_move_to_todo_queues_one_run() {
    let k = kernel().await;
    let ada = k.agent("Ada").await;
    let task = k.task(ada, TaskStatus::Todo).await;
    k.working(task).await;
    k.engine
        .move_task_manually(task, TaskStatus::Cancelled, None)
        .await
        .unwrap();
    k.until(task, TaskStatus::Cancelled).await;
    let before = k.runs(task).await.len();
    let mover = {
        let engine = k.engine.clone();
        tokio::spawn(async move {
            engine
                .move_task_manually(task, TaskStatus::Todo, None)
                .await
        })
    };
    storm(&k, task, "race").await;
    // The move may lose the race (the task is already Todo): refused, no run.
    let _ = mover.await.unwrap();
    let runs = k.runs(task).await;
    assert!(runs.len() <= before + 1, "{runs:?}");
    assert!(live_runs(&k, task).await <= 1);
    k.until(task, TaskStatus::InProgress).await;
    assert_eq!(live_runs(&k, task).await, 1);
}

/// An agent and the runtimes its chain names belong to its workspace: the
/// same chain name is the workspace's own runtime (never Main's row of that
/// name), and no other workspace's task can be assigned to it.
#[tokio::test(flavor = "multi_thread")]
async fn agents_and_their_runtimes_stay_in_their_workspace() {
    let k = kernel().await;
    let policy = sun_core::model::SignupPolicy::default();
    let ops = k
        .engine
        .create_workspace("Ops", None, None, &policy, "user:me")
        .await
        .unwrap()
        .id;
    // Main's `fake` runs another adapter; Ops' chain must not resolve to it.
    let mut tx = k.store.begin().await.unwrap();
    tx.upsert_runtime(
        WorkspaceId::MAIN,
        "fake",
        "other-adapter",
        sun_core::model::RuntimeSource::ByoRuntime,
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    let fields = || AgentFields {
        name: Some("Ada".into()),
        runtime_chain: Some(vec!["fake".into()]),
        ..AgentFields::default()
    };
    let main_ada = k.agent("Ada").await;
    let ops_ada = k
        .engine
        .create_agent(ops, fields(), "user:me", &crate::support::unlimited())
        .await
        .unwrap();
    assert_eq!(ops_ada.workspace_id, ops, "one name per workspace");
    assert_eq!(
        k.engine.agents(Some(ops)).await.unwrap(),
        std::slice::from_ref(&ops_ada)
    );

    // A Main task can't take Ops' agent, on create or reassignment.
    let mut new = NewTask {
        workspace_id: Some(WorkspaceId::MAIN),
        title: "Fix the login".into(),
        body: String::new(),
        workflow: WorkflowPick::Agent(ops_ada.id),
        exec_env: None,
        priority: Priority::None,
        status: TaskStatus::Backlog,
        project_id: None,
        actor: None,
    };
    let err = k.engine.create_task(new.clone()).await.unwrap_err();
    assert!(matches!(err, Error::NotFound(_)), "{err:?}");
    let main_task = k.task(main_ada, TaskStatus::Backlog).await;
    let to = Some(Assignee::Agent { id: ops_ada.id });
    let err = k.engine.assign_task(main_task, to, None).await.unwrap_err();
    assert!(matches!(err, Error::NotFound(_)), "{err:?}");

    // Ops' task waits while Ops has no host (the only one is Main's), then
    // runs on Ops' own `fake` with only Ops' skills.
    let mut tx = k.store.begin().await.unwrap();
    tx.link(WorkspaceId::MAIN, WorkspaceItem::CatalogInstall("tdd"))
        .await
        .unwrap();
    tx.link(ops, WorkspaceItem::CatalogInstall("playwright"))
        .await
        .unwrap();
    tx.commit().await.unwrap();
    new.workspace_id = Some(ops);
    new.status = TaskStatus::Todo;
    let task = k.engine.create_task(new).await.unwrap().id;
    tokio::time::sleep(Duration::from_millis(300)).await;
    let queued = k.runs(task).await.pop().unwrap().status;
    assert_eq!(queued, RunStatus::Queued, "not on Main's host");
    assert!(k.host.adapters.lock().unwrap().is_empty());
    let host = WorkspaceItem::Host(k.host_id);
    k.engine
        .import_to_workspace(ops, host, "user:me", &crate::support::unlimited())
        .await
        .unwrap();
    k.queue.wake();
    k.working(task).await;
    assert_eq!(*k.host.adapters.lock().unwrap(), ["fake"]);
    assert_eq!(
        *k.host.skills.lock().unwrap(),
        [Some(vec!["playwright".to_owned()])]
    );
    let run = k.runs(task).await.pop().unwrap();
    let mut read = k.store.read().await.unwrap();
    let step = read.steps_of_run(run.id).await.unwrap().pop().unwrap();
    let runtime = read.get_runtime(step.runtime_id.unwrap()).await.unwrap();
    assert_eq!(
        (runtime.workspace_id, runtime.adapter.as_str()),
        (ops, "fake")
    );
    k.host.finish.store(true, Ordering::SeqCst);
}

/// A run's context gets only its own workspace's memory: with an active
/// rule in Main and another in Ops, an Ops task's session reads Ops' rule
/// and never Main's, and the other way round.
#[tokio::test(flavor = "multi_thread")]
async fn a_run_gets_only_its_workspaces_memory() {
    use sun_core::model::{MemoryItem, MemoryScope, MemoryStatus};
    use sun_store::MemoryRepo;

    let k = kernel().await;
    let policy = sun_core::model::SignupPolicy::default();
    let ops = k
        .engine
        .create_workspace("Ops", None, None, &policy, "user:me")
        .await
        .unwrap()
        .id;
    k.engine
        .import_to_workspace(
            ops,
            WorkspaceItem::Host(k.host_id),
            "user:me",
            &crate::support::unlimited(),
        )
        .await
        .unwrap();
    let mut tx = k.store.begin().await.unwrap();
    for (workspace, body) in [
        (WorkspaceId::MAIN, "Main rule: deploy with blue-green."),
        (ops, "Ops rule: page the on-call lead."),
    ] {
        let item = MemoryItem {
            id: sun_core::MemoryItemId::generate(),
            workspace_id: workspace,
            scope: MemoryScope::Ecosystem,
            scope_id: None,
            body: body.into(),
            target: None,
            reason: "r".into(),
            source_run: None,
            status: MemoryStatus::Active,
            author: "sun".into(),
            reviewed_by: None,
            created_at: chrono::Utc::now(),
        };
        tx.insert_memory(&item, body).await.unwrap();
    }
    tx.commit().await.unwrap();
    let fields = AgentFields {
        name: Some("Ada".into()),
        runtime_chain: Some(vec!["fake".into()]),
        ..AgentFields::default()
    };
    let ops_ada = k
        .engine
        .create_agent(ops, fields, "user:me", &crate::support::unlimited())
        .await
        .unwrap()
        .id;
    let new = NewTask {
        workspace_id: Some(ops),
        title: "Fix the login".into(),
        body: String::new(),
        workflow: WorkflowPick::Agent(ops_ada),
        exec_env: None,
        priority: Priority::None,
        status: TaskStatus::Todo,
        project_id: None,
        actor: None,
    };
    let ops_task = k.engine.create_task(new).await.unwrap().id;
    k.working(ops_task).await;
    let main_ada = k.agent("Ada").await;
    k.task(main_ada, TaskStatus::Todo).await;
    eventually("both sessions", || async { k.host.envelopes().len() == 2 }).await;
    // Ops' session started first (it was working before Main's task).
    let envelopes = k.host.envelopes();
    let (in_ops, in_main) = (&envelopes[0], &envelopes[1]);
    assert!(in_ops.contains("Ops rule"), "{in_ops}");
    assert!(!in_ops.contains("Main rule"), "{in_ops}");
    assert!(in_main.contains("Main rule"), "{in_main}");
    assert!(!in_main.contains("Ops rule"), "{in_main}");
    k.host.finish.store(true, Ordering::SeqCst);
}

// BLO-128 part 1 test pass: reply-as-comment once, wake-nobody, resume
// gate, fresh-fallback thread bounds, preferred host, queued author.

/// Long enough for anything the engine would do with an event already
/// sent to have happened: for "and nothing else happens" checks only.
async fn settle() {
    tokio::time::sleep(Duration::from_millis(300)).await;
}

fn actor(name: &str) -> Option<Actor> {
    Some(Actor {
        member_id: None,
        name: name.into(),
    })
}

/// The `- who: text` lines of the thread block in a session's prompt.
fn thread_lines(envelope: &str) -> Vec<String> {
    const HEAD: &str = "The conversation on this task so far, oldest first:\n";
    let Some(at) = envelope.find(HEAD) else {
        return Vec::new();
    };
    let block = &envelope[at + HEAD.len()..];
    let block = block.split("\n\n").next().unwrap_or_default();
    block.lines().map(str::to_owned).collect()
}

impl Kernel {
    /// The texts of the task's comments, oldest first.
    async fn texts(&self, task: TaskId) -> Vec<String> {
        let comments = self.comments(task).await;
        comments
            .iter()
            .map(|c| c["text"].as_str().unwrap().to_owned())
            .collect()
    }

    /// Writes a comment row straight to the task's log (no wake).
    async fn seed(&self, task: TaskId, body: Value) {
        use sun_core::model::ActivityKind;
        let mut tx = self.store.begin().await.unwrap();
        tx.append_activity(task, ActivityKind::Comment, None, body)
            .await
            .unwrap();
        tx.commit().await.unwrap();
    }

    /// Waits until `run`'s session has said something (its `text` event is
    /// stored): a done claim before any output is the watchdog's to fail.
    async fn spoke(&self, run: sun_core::RunId) {
        use sun_store::EventLog;
        eventually("the session's first output", || async {
            let mut tx = self.store.read().await.unwrap();
            let events = tx.events_after(Some(run), 0, 1000).await.unwrap();
            events.iter().any(|e| e.kind == "text")
        })
        .await;
    }

    async fn say(&self, task: TaskId, who: &str, text: &str) {
        self.engine
            .comment(task, text, actor(who), &[])
            .await
            .unwrap();
    }

    /// The `n`th session's `(resumed native id, host)`.
    fn session(&self, n: usize) -> (Option<String>, HostId) {
        let resume = self.host.natives.lock().unwrap()[n].0.clone();
        (resume, self.host.started_on.lock().unwrap()[n])
    }

    async fn sessions(&self, n: usize) {
        eventually(&format!("{n} sessions"), || async {
            self.host.envelopes().len() == n
        })
        .await;
    }

    fn set_online(&self, host: HostId, online: bool) {
        let mut offline = self.host.offline.lock().unwrap();
        if online {
            offline.remove(&host);
        } else {
            offline.insert(host);
        }
        drop(offline);
        self.queue.wake();
    }
}

/// 1. A done claim is one comment however often it arrives: the host
/// redelivers each event, the CLI claims twice, and `sun.done` races the
/// host's own claim either way round.
#[tokio::test(flavor = "multi_thread")]
async fn a_done_claim_is_one_comment_however_often_it_arrives() {
    let k = kernel().await;
    let ada = k.agent("Ada").await;
    k.host.redeliver.store(true, Ordering::SeqCst);
    *k.host.claims.lock().unwrap() = vec!["done".into(), "done, again".into()];
    k.host.finish.store(true, Ordering::SeqCst);
    let task = k.task(ada, TaskStatus::Todo).await;
    k.until(task, TaskStatus::NeedsYou).await;
    settle().await;
    assert_eq!(k.texts(task).await, ["done"]);

    // `sun.done` first, then the session's own claims (twice each).
    k.host.finish.store(false, Ordering::SeqCst);
    k.say(task, "user:ana", "Also fix signup").await;
    k.until(task, TaskStatus::InProgress).await;
    k.working(task).await;
    let run = k.runs(task).await[1].id;
    k.spoke(run).await;
    let step = sun_core::StepKey::new(sun_engine::AGENT_STEP).unwrap();
    k.engine
        .done(run, step.clone(), "Fixed signup too.".into())
        .await
        .unwrap();
    k.until(task, TaskStatus::NeedsYou).await;
    k.host.finish.store(true, Ordering::SeqCst);
    settle().await;
    assert_eq!(
        k.texts(task).await,
        ["done", "Also fix signup", "Fixed signup too."]
    );

    // The session's claim first, then a late `sun.done`: refused, no comment.
    k.say(task, "user:ana", "And the logout").await;
    eventually("the third reply", || async {
        k.texts(task).await.len() == 5
    })
    .await;
    let run = k.runs(task).await[2].id;
    k.spoke(run).await;
    let late = k.engine.done(run, step, "late".into()).await;
    assert!(late.is_err(), "{late:?}");
    settle().await;
    assert_eq!(
        k.texts(task).await,
        [
            "done",
            "Also fix signup",
            "Fixed signup too.",
            "And the logout",
            "done"
        ]
    );
    assert_eq!(k.runs(task).await.len(), 3);
}

/// 1. Empty or whitespace-only claims post nothing, on both claim paths.
#[tokio::test(flavor = "multi_thread")]
async fn an_empty_or_blank_done_claim_posts_no_comment() {
    let k = kernel().await;
    let ada = k.agent("Ada").await;
    *k.host.claims.lock().unwrap() = vec![" \n\t ".into()];
    k.host.finish.store(true, Ordering::SeqCst);
    let task = k.task(ada, TaskStatus::Todo).await;
    k.until(task, TaskStatus::NeedsYou).await;
    *k.host.claims.lock().unwrap() = vec![String::new()];
    let other = k.task(ada, TaskStatus::Todo).await;
    k.until(other, TaskStatus::NeedsYou).await;
    settle().await;
    assert!(k.comments(task).await.is_empty());
    assert!(k.comments(other).await.is_empty());

    // `sun.done` with a blank summary.
    k.host.finish.store(false, Ordering::SeqCst);
    k.say(task, "user:ana", "Go on").await;
    k.until(task, TaskStatus::InProgress).await;
    k.working(task).await;
    let run = k.runs(task).await[1].id;
    k.spoke(run).await;
    let step = sun_core::StepKey::new(sun_engine::AGENT_STEP).unwrap();
    k.engine.done(run, step, "   ".into()).await.unwrap();
    k.until(task, TaskStatus::NeedsYou).await;
    k.host.finish.store(true, Ordering::SeqCst);
    settle().await;
    assert_eq!(k.texts(task).await, ["Go on"]);
}

/// 1. A run of a workflow that is no agent's posts no comment, though its
/// agent step claims done like any other.
#[tokio::test(flavor = "multi_thread")]
async fn a_non_agent_workflows_done_claim_posts_no_comment() {
    use sun_core::StateMachine;
    let k = kernel().await;
    let (workflow, _) = k
        .engine
        .save_workflow(
            None,
            "name: build\nsteps:\n  - id: build\n    type: agent\n    prompt: build it\n",
        )
        .await
        .unwrap();
    k.host.finish.store(true, Ordering::SeqCst);
    let new = NewTask {
        workspace_id: Some(WorkspaceId::MAIN),
        title: "Build".into(),
        body: String::new(),
        workflow: WorkflowPick::Id(workflow.id),
        exec_env: None,
        priority: Priority::None,
        status: TaskStatus::Todo,
        project_id: None,
        actor: None,
    };
    let task = k.engine.create_task(new).await.unwrap().id;
    eventually("the run ends", || async {
        k.runs(task)
            .await
            .first()
            .is_some_and(|r| r.status.is_terminal())
    })
    .await;
    assert_eq!(k.host.envelopes().len(), 1, "its agent step ran");
    settle().await;
    assert!(k.comments(task).await.is_empty());
}

/// 1. A claim longer than a comment may be is cut at `MAX_COMMENT` chars
/// (not bytes) after trimming, never refused; the claim stays whole in
/// the run's log.
#[tokio::test(flavor = "multi_thread")]
async fn a_done_claim_longer_than_a_comment_is_truncated_not_rejected() {
    use sun_core::model::TaskActivity;
    let k = kernel().await;
    let ada = k.agent("Ada").await;
    let long = format!("\n  {}", "é".repeat(TaskActivity::MAX_COMMENT + 50));
    *k.host.claims.lock().unwrap() = vec![long];
    k.host.finish.store(true, Ordering::SeqCst);
    let task = k.task(ada, TaskStatus::Todo).await;
    k.until(task, TaskStatus::NeedsYou).await;
    let texts = k.texts(task).await;
    assert_eq!(texts.len(), 1);
    assert_eq!(texts[0].chars().count(), TaskActivity::MAX_COMMENT);
    assert!(texts[0].chars().all(|c| c == 'é'));

    // Exactly at the limit: kept whole.
    *k.host.claims.lock().unwrap() = vec!["a".repeat(TaskActivity::MAX_COMMENT)];
    let exact = k.task(ada, TaskStatus::Todo).await;
    k.until(exact, TaskStatus::NeedsYou).await;
    assert_eq!(
        k.texts(exact).await,
        ["a".repeat(TaskActivity::MAX_COMMENT)]
    );
}

/// 1. Secrets in the claim are redacted in the comment like in every other
/// activity write.
#[tokio::test(flavor = "multi_thread")]
async fn secrets_in_a_done_claim_are_redacted_in_its_comment() {
    let k = kernel().await;
    let ada = k.agent("Ada").await;
    let token = "ghp_0123456789abcdefghijKLMNOP";
    let claim =
        format!("Pushed with {token}; set API_KEY=hunter2hunter2 and Bearer abcdefghijklmnopqrstu");
    *k.host.claims.lock().unwrap() = vec![claim];
    k.host.finish.store(true, Ordering::SeqCst);
    let task = k.task(ada, TaskStatus::Todo).await;
    k.until(task, TaskStatus::NeedsYou).await;
    let text = k.texts(task).await.pop().unwrap();
    for secret in [token, "hunter2hunter2", "abcdefghijklmnopqrstu"] {
        assert!(!text.contains(secret), "{secret} leaked: {text}");
    }
    assert!(text.starts_with("Pushed with ghp_[REDACTED]"), "{text}");

    // `sun.done`'s summary goes through the same filter.
    k.host.finish.store(false, Ordering::SeqCst);
    k.say(task, "user:ana", "Again").await;
    k.until(task, TaskStatus::InProgress).await;
    k.working(task).await;
    let run = k.runs(task).await[1].id;
    k.spoke(run).await;
    let step = sun_core::StepKey::new(sun_engine::AGENT_STEP).unwrap();
    k.engine
        .done(run, step, format!("token {token}"))
        .await
        .unwrap();
    k.until(task, TaskStatus::NeedsYou).await;
    k.host.finish.store(true, Ordering::SeqCst);
    let text = k.texts(task).await.pop().unwrap();
    assert_eq!(text, "token ghp_[REDACTED]");
}

/// 2. An agent's reply queues no run on its task and wakes no other agent,
/// even when it names one.
#[tokio::test(flavor = "multi_thread")]
async fn an_agents_reply_wakes_no_agent_anywhere() {
    use sun_core::StateMachine;
    let k = kernel().await;
    let (ada, bob) = (k.agent("Ada").await, k.agent("Bob").await);
    k.host.finish.store(true, Ordering::SeqCst);
    let bobs = k.task(bob, TaskStatus::Todo).await;
    k.until(bobs, TaskStatus::NeedsYou).await;
    *k.host.claims.lock().unwrap() = vec!["@Bob please review MAIN-1".into()];
    let adas = k.task(ada, TaskStatus::Todo).await;
    k.until(adas, TaskStatus::NeedsYou).await;
    k.say(adas, "user:ana", "Thanks").await;
    eventually("Ada's second reply", || async {
        k.texts(adas).await.len() == 3
    })
    .await;
    settle().await;
    assert_eq!(k.runs(bobs).await.len(), 1, "Bob is not woken");
    assert_eq!(k.runs(adas).await.len(), 2, "one run per human comment");
    for task in [adas, bobs] {
        assert!(k.runs(task).await.iter().all(|r| r.status.is_terminal()));
    }
    assert_eq!(k.host.envelopes().len(), 3);
    let activity = k.engine.activity(adas, 0).await.unwrap();
    assert!(activity.iter().all(|a| a.actor_member_id.is_none()));
}

/// 2. Human, agent, human, agent: one run per human comment and never two
/// runs of the task live at once.
#[tokio::test(flavor = "multi_thread")]
async fn a_back_and_forth_never_fans_out_to_more_than_one_run() {
    let k = kernel().await;
    let ada = k.agent("Ada").await;
    k.host.finish.store(true, Ordering::SeqCst);
    let task = k.task(ada, TaskStatus::Todo).await;
    k.until(task, TaskStatus::NeedsYou).await;
    // Watch the task's live runs for the whole exchange.
    let peak = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let stop = Arc::new(AtomicBool::new(false));
    let watcher = {
        let (store, peak, stop) = (k.store.clone(), peak.clone(), stop.clone());
        tokio::spawn(async move {
            use sun_core::StateMachine;
            while !stop.load(Ordering::SeqCst) {
                let runs = store
                    .read()
                    .await
                    .unwrap()
                    .runs_of_task(task)
                    .await
                    .unwrap();
                let live = runs.iter().filter(|r| !r.status.is_terminal()).count();
                peak.fetch_max(live, Ordering::SeqCst);
                tokio::task::yield_now().await;
            }
        })
    };
    for (i, text) in ["First question", "Second question"].iter().enumerate() {
        k.say(task, "user:ana", text).await;
        let want = 3 + 2 * i;
        eventually("the reply", || async { k.texts(task).await.len() == want }).await;
    }
    settle().await;
    stop.store(true, Ordering::SeqCst);
    watcher.await.unwrap();
    assert_eq!(
        k.texts(task).await,
        ["done", "First question", "done", "Second question", "done"]
    );
    assert_eq!(k.runs(task).await.len(), 3);
    assert_eq!(peak.load(Ordering::SeqCst), 1);
}

/// 3. A comment-woken run whose previous run was another workflow (the task
/// was reassigned) starts fresh, and reads the other agent's reply under
/// its name, not "(you)". Its own next comment then resumes it (control).
#[tokio::test(flavor = "multi_thread")]
async fn resume_is_refused_when_the_previous_run_was_another_workflow() {
    let k = kernel().await;
    k.host.native.store(true, Ordering::SeqCst);
    let (ada, bob) = (k.agent("Ada").await, k.agent("Bob").await);
    k.host.finish.store(true, Ordering::SeqCst);
    let task = k.task(ada, TaskStatus::Todo).await;
    k.until(task, TaskStatus::NeedsYou).await;
    // Parked, then handed to Bob: no run until someone comments.
    k.engine
        .move_task_manually(task, TaskStatus::Blocked, None)
        .await
        .unwrap();
    let to_bob = Some(Assignee::Agent { id: bob });
    k.engine.assign_task(task, to_bob, None).await.unwrap();
    assert_eq!(k.runs(task).await.len(), 1);
    k.say(task, "user:ana", "Bob, take over").await;
    k.sessions(2).await;
    assert_eq!(k.session(1).0, None, "Bob's session is fresh");
    let fresh = k.host.envelopes()[1].clone();
    assert!(fresh.contains("You are Bob."), "{fresh}");
    assert_eq!(
        thread_lines(&fresh),
        ["- Ada: done", "- ana: Bob, take over"]
    );
    k.until(task, TaskStatus::NeedsYou).await;
    k.say(task, "user:ana", "Thanks Bob").await;
    k.sessions(3).await;
    let bobs_first = k.host.natives.lock().unwrap()[1].1.clone();
    assert_eq!(k.session(2).0, bobs_first, "Bob's own session resumes");
}

/// 3. A comment-woken run on another runtime than the previous run's starts
/// fresh; the next one, on the same runtime, resumes (control).
#[tokio::test(flavor = "multi_thread")]
async fn resume_is_refused_when_the_previous_run_was_another_runtime() {
    let k = kernel().await;
    k.host.native.store(true, Ordering::SeqCst);
    let ada = k.agent("Ada").await;
    k.host.finish.store(true, Ordering::SeqCst);
    let task = k.task(ada, TaskStatus::Todo).await;
    k.until(task, TaskStatus::NeedsYou).await;
    let chain = AgentFields {
        runtime_chain: Some(vec!["fake2".into()]),
        ..AgentFields::default()
    };
    k.engine.update_agent(ada, chain, "user:me").await.unwrap();
    k.say(task, "user:ana", "On the other CLI").await;
    k.sessions(2).await;
    assert_eq!(*k.host.adapters.lock().unwrap(), ["fake", "fake2"]);
    assert_eq!(k.session(1).0, None, "another runtime: fresh");
    let fresh = k.host.envelopes()[1].clone();
    assert!(fresh.contains("Your task: MAIN-1"), "{fresh}");
    assert_eq!(
        thread_lines(&fresh),
        ["- Ada (you): done", "- ana: On the other CLI"]
    );
    k.until(task, TaskStatus::NeedsYou).await;
    k.say(task, "user:ana", "Same CLI now").await;
    k.sessions(3).await;
    let second = k.host.natives.lock().unwrap()[1].1.clone();
    assert_eq!(k.session(2).0, second);
}

/// 3. The watchdog stopped the previous run (here: looping): the
/// comment-woken run starts fresh with the thread instead of carrying that
/// session on.
#[tokio::test(flavor = "multi_thread")]
async fn resume_is_refused_when_the_watchdog_stopped_the_previous_run() {
    let k = kernel().await;
    k.host.native.store(true, Ordering::SeqCst);
    k.host.looping.store(true, Ordering::SeqCst);
    let ada = k.agent("Ada").await;
    let task = k.task(ada, TaskStatus::Todo).await;
    eventually("a looping run", || async {
        k.runs(task).await.first().map(|r| r.status) == Some(RunStatus::Looping)
    })
    .await;
    k.host.looping.store(false, Ordering::SeqCst);
    k.host.finish.store(true, Ordering::SeqCst);
    k.say(task, "user:ana", "Try again").await;
    k.sessions(2).await;
    assert_eq!(k.session(1).0, None, "fresh after a watchdog stop");
    let fresh = k.host.envelopes()[1].clone();
    assert!(fresh.contains("Your task: MAIN-1"), "{fresh}");
    assert_eq!(thread_lines(&fresh), ["- ana: Try again"]);
}

/// 3 and 5. Two hosts: a comment-woken run goes to the previous run's host
/// while it has room, even when that one is busier, and resumes there;
/// when that host is full it runs elsewhere, fresh; when it is offline,
/// the same.
#[tokio::test(flavor = "multi_thread")]
async fn a_comment_run_prefers_its_last_host_and_resumes_only_there() {
    let caps = sun_engine::Caps {
        per_host: std::collections::BTreeMap::from([(
            "*".to_owned(),
            sun_engine::HostCap {
                runs: Some(2),
                ..Default::default()
            },
        )]),
        ..Default::default()
    };
    let k = kernel_on(&[("a", Some(&[])), ("b", Some(&[]))], caps).await;
    let (a, b) = (k.hosts[0], k.hosts[1]);
    k.host.native.store(true, Ordering::SeqCst);
    let ada = k.agent("Ada").await;
    let (gate, _) = k
        .engine
        .save_workflow(
            None,
            "name: gate\nsteps:\n  - id: approve\n    type: approval\n    who: [role:admin]\n",
        )
        .await
        .unwrap();
    // A run that holds a slot until cancelled, on `on`.
    let hold = |on: HostId| {
        let k = &k;
        async move {
            let other = if on == a { b } else { a };
            k.set_online(other, false);
            let new = NewTask {
                workspace_id: Some(WorkspaceId::MAIN),
                title: "Hold".into(),
                body: String::new(),
                workflow: WorkflowPick::Id(gate.id),
                exec_env: None,
                priority: Priority::None,
                status: TaskStatus::Todo,
                project_id: None,
                actor: None,
            };
            let task = k.engine.create_task(new).await.unwrap().id;
            eventually("the hold running", || async {
                k.runs(task)
                    .await
                    .first()
                    .is_some_and(|r| r.status == RunStatus::Running)
            })
            .await;
            k.set_online(other, true);
            task
        }
    };

    // Ada's first run lands on a.
    k.set_online(b, false);
    k.host.finish.store(true, Ordering::SeqCst);
    let task = k.task(ada, TaskStatus::Todo).await;
    k.until(task, TaskStatus::NeedsYou).await;
    k.set_online(b, true);
    assert_eq!(k.session(0), (None, a));

    // a is full: the comment's run goes to b, fresh (another host).
    let holds = [hold(a).await, hold(a).await];
    k.say(task, "user:ana", "One").await;
    k.sessions(2).await;
    assert_eq!(k.session(1), (None, b));
    let fresh = k.host.envelopes()[1].clone();
    assert_eq!(thread_lines(&fresh), ["- Ada (you): done", "- ana: One"]);
    k.until(task, TaskStatus::NeedsYou).await;

    // a is empty, b is busier: the next comment still goes to b and resumes.
    for held in holds {
        k.engine
            .move_task_manually(held, TaskStatus::Cancelled, None)
            .await
            .unwrap();
    }
    let busy_b = hold(b).await;
    k.say(task, "user:ana", "Two").await;
    k.sessions(3).await;
    let on_b = k.host.natives.lock().unwrap()[1].1.clone();
    assert!(on_b.is_some());
    assert_eq!(k.session(2), (on_b, b), "prefers b and resumes there");
    k.until(task, TaskStatus::NeedsYou).await;

    // b is offline now: a, fresh.
    k.engine
        .move_task_manually(busy_b, TaskStatus::Cancelled, None)
        .await
        .unwrap();
    k.set_online(b, false);
    k.say(task, "user:ana", "Three").await;
    k.sessions(4).await;
    assert_eq!(k.session(3), (None, a));
    let fresh = k.host.envelopes()[3].clone();
    assert!(fresh.contains("Your task: MAIN-1"), "{fresh}");
}

/// 5. A container run that a comment woke goes only to a host with
/// containers ready: not its previous run's host once that one stops
/// reporting them, and not a bare-only host, both with room.
#[tokio::test(flavor = "multi_thread")]
async fn a_comment_run_needing_a_container_skips_a_preferred_host_without_one() {
    let k = kernel_on(
        &[
            ("plain", Some(&[])),
            ("docker", Some(&["container"])),
            ("docker2", Some(&["container"])),
        ],
        sun_engine::Caps::default(),
    )
    .await;
    let (plain, docker, docker2) = (k.hosts[0], k.hosts[1], k.hosts[2]);
    k.host.native.store(true, Ordering::SeqCst);
    let ada = k.agent("Ada").await;
    k.host.finish.store(true, Ordering::SeqCst);
    let container: sun_core::ExecEnvSpec =
        serde_json::from_value(json!({"kind": "container", "image": "node:22"})).unwrap();
    k.set_online(docker2, false);
    let new = NewTask {
        workspace_id: Some(WorkspaceId::MAIN),
        title: "Fix the login".into(),
        body: String::new(),
        workflow: WorkflowPick::Agent(ada),
        exec_env: Some(container),
        priority: Priority::None,
        status: TaskStatus::Todo,
        project_id: None,
        actor: None,
    };
    let task = k.engine.create_task(new).await.unwrap().id;
    k.until(task, TaskStatus::NeedsYou).await;
    k.set_online(docker2, true);
    assert_eq!(k.session(0).1, docker, "the only host with containers");
    // docker's container engine went away; it is online with free slots.
    let bare_only = json!({
        "os": "linux", "arch": "x86_64", "cpus": 4,
        "runtimes": [{"adapter": "fake", "readiness":
            {"installed": true, "version": "1", "logged_in": true, "detail": null}}],
    });
    k.engine
        .set_host_capabilities(docker, &bare_only)
        .await
        .unwrap();
    k.say(task, "user:ana", "Again").await;
    k.sessions(2).await;
    let (resume, on) = k.session(1);
    assert_ne!(on, plain, "a bare-only host never takes a container run");
    assert_eq!((resume, on), (None, docker2));
}

/// 3. The previous run never got a session (cancelled before dispatch):
/// the comment-woken run starts fresh with the task and the thread.
#[tokio::test(flavor = "multi_thread")]
async fn resume_is_refused_when_the_previous_run_had_no_session() {
    let k = kernel_on(&[("a", Some(&[]))], sun_engine::Caps::default()).await;
    k.host.native.store(true, Ordering::SeqCst);
    let ada = k.agent("Ada").await;
    k.set_online(k.host_id, false);
    let task = k.task(ada, TaskStatus::Todo).await;
    eventually("a queued run", || async { k.runs(task).await.len() == 1 }).await;
    k.engine
        .move_task_manually(task, TaskStatus::Blocked, None)
        .await
        .unwrap();
    k.until(task, TaskStatus::Blocked).await;
    assert_eq!(k.runs(task).await[0].status, RunStatus::Cancelled);
    k.set_online(k.host_id, true);
    k.say(task, "user:ana", "Now please").await;
    k.sessions(1).await;
    assert_eq!(k.session(0).0, None);
    let fresh = k.host.envelopes()[0].clone();
    assert!(fresh.contains("Your task: MAIN-1"), "{fresh}");
    assert_eq!(thread_lines(&fresh), ["- ana: Now please"]);
    k.host.finish.store(true, Ordering::SeqCst);
}

/// 4. The fresh fallback's thread: the last 20 comments; then trimmed from
/// the oldest end to 16k; the agent's own marked "(you)"; any `by` shape
/// still names someone.
#[tokio::test(flavor = "multi_thread")]
async fn the_fresh_fallback_thread_is_bounded_and_names_everyone() {
    let k = kernel().await;
    let ada = k.agent("Ada").await;
    k.host.finish.store(true, Ordering::SeqCst);

    // 25 comments (+ Ada's reply + the waking one): the last 20 only.
    let task = k.task(ada, TaskStatus::Todo).await;
    k.until(task, TaskStatus::NeedsYou).await;
    for i in 1..=25 {
        let text = format!("c{i:02}");
        k.seed(task, json!({ "text": text, "by": "user:ana" }))
            .await;
    }
    k.say(task, "user:ana", "wake").await;
    k.sessions(2).await;
    let lines = thread_lines(&k.host.envelopes()[1]);
    let mut want: Vec<String> = (7..=25).map(|i| format!("- ana: c{i:02}")).collect();
    want.push("- ana: wake".into());
    assert_eq!(lines, want);
    k.until(task, TaskStatus::NeedsYou).await;

    // 20 comments of 1000 chars: trimmed from the oldest end to 16k.
    let long = k.task(ada, TaskStatus::Todo).await;
    k.until(long, TaskStatus::NeedsYou).await;
    for i in 1..=20 {
        let text = format!("{i:02}{}", "x".repeat(998));
        k.seed(long, json!({ "text": text, "by": "user:ana" }))
            .await;
    }
    k.say(long, "user:ana", "wake").await;
    k.sessions(4).await;
    let lines = thread_lines(&k.host.envelopes()[3]);
    let size: usize = lines.iter().map(String::len).sum();
    assert!(size <= 16_000, "{size}");
    assert_eq!(lines.len(), 16, "15 long ones and the waking one");
    assert!(lines[0].starts_with("- ana: 06x"), "{}", &lines[0][..12]);
    assert_eq!(lines[15], "- ana: wake");
    k.until(long, TaskStatus::NeedsYou).await;

    // Names: the agent's own, a bare `by`, a mod actor, and no `by` at all.
    let names = k.task(ada, TaskStatus::Todo).await;
    k.until(names, TaskStatus::NeedsYou).await;
    k.seed(names, json!({ "text": "plain", "by": "ana" })).await;
    k.seed(names, json!({ "text": "beep", "by": "mod:github" }))
        .await;
    k.seed(names, json!({ "text": "who?" })).await;
    k.say(names, "user:ana", "wake").await;
    k.sessions(6).await;
    assert_eq!(
        thread_lines(&k.host.envelopes()[5]),
        [
            "- Ada (you): done",
            "- ana: plain",
            "- github: beep",
            "- someone: who?",
            "- ana: wake"
        ]
    );
}

/// 6. A message left for a later session says who wrote it (`- ana: text`);
/// one without an actor has no prefix; a steer into a live session never
/// gets one.
#[tokio::test(flavor = "multi_thread")]
async fn a_queued_message_names_its_author_and_a_steer_does_not() {
    let k = kernel_on(&[("a", Some(&[]))], sun_engine::Caps::default()).await;
    let ada = k.agent("Ada").await;
    k.set_online(k.host_id, false);
    let task = k.task(ada, TaskStatus::Todo).await;
    eventually("a queued run", || async { k.runs(task).await.len() == 1 }).await;
    k.say(task, "user:ana", "Use the v2 API").await;
    k.comment(task, "From nobody").await;
    k.say(task, "bob", "Bare name").await;
    k.set_online(k.host_id, true);
    k.sessions(1).await;
    let first = k.host.envelopes()[0].clone();
    assert!(
        first.contains(
            "Messages left for this step:\n- ana: Use the v2 API\n- From nobody\n- bob: Bare name"
        ),
        "{first}"
    );
    k.working(task).await;
    k.say(task, "user:ana", "Steer me").await;
    assert_eq!(*k.host.steers.lock().unwrap(), ["Steer me"]);
    k.host.finish.store(true, Ordering::SeqCst);
}

/// 1. Defect probe: the claim is cut to `MAX_COMMENT` chars before the
/// secret filter runs, so a secret straddling the cut is no longer whole
/// (or no longer long enough) and its head is posted in clear.
#[tokio::test(flavor = "multi_thread")]
async fn a_secret_cut_by_the_comment_limit_is_still_redacted() {
    use sun_core::model::{SecretScope, TaskActivity};
    let k = kernel().await;
    let scope = SecretScope::Ecosystem(sun_core::EcosystemId::generate());
    let secret = "S3cret-Value-0042XYZ";
    k.engine
        .set_secret(&scope, "deploy", secret, "user:me")
        .await
        .unwrap();
    let ada = k.agent("Ada").await;
    // The cut falls on the secret's last char, then on a token's tail.
    let pad = "a".repeat(TaskActivity::MAX_COMMENT - secret.len() + 1);
    let token = "ghp_0123456789abcdefghijKLMNOP";
    // 19 of the token's chars fit: `ghp_` and a 15-char tail, too short to
    // read as a token.
    let pad2 = format!("{} ", "a".repeat(TaskActivity::MAX_COMMENT - 20));
    *k.host.claims.lock().unwrap() = vec![format!("{pad}{secret}")];
    k.host.finish.store(true, Ordering::SeqCst);
    let task = k.task(ada, TaskStatus::Todo).await;
    k.until(task, TaskStatus::NeedsYou).await;
    *k.host.claims.lock().unwrap() = vec![format!("{pad2}{token}")];
    let other = k.task(ada, TaskStatus::Todo).await;
    k.until(other, TaskStatus::NeedsYou).await;
    let cut_secret = k.texts(task).await.pop().unwrap();
    let cut_token = k.texts(other).await.pop().unwrap();
    assert!(
        !cut_secret.contains(&secret[..secret.len() - 1]),
        "secret head posted: …{}",
        &cut_secret[cut_secret.len() - 24..]
    );
    assert!(
        !cut_token.contains(&token[..19]),
        "token head posted: …{}",
        &cut_token[cut_token.len() - 24..]
    );
}

/// 4. Defect probe: the thread's 16k budget counts bytes, not chars, so a
/// non-ASCII thread well under 16k chars is trimmed anyway.
#[tokio::test(flavor = "multi_thread")]
async fn the_thread_budget_counts_chars_not_bytes() {
    let k = kernel().await;
    let ada = k.agent("Ada").await;
    k.host.finish.store(true, Ordering::SeqCst);
    let task = k.task(ada, TaskStatus::Todo).await;
    k.until(task, TaskStatus::NeedsYou).await;
    // 10 comments of 1000 'é': 10k chars (20k bytes), under 16k chars.
    for i in 0..10 {
        let text = format!("{i}{}", "é".repeat(999));
        k.seed(task, json!({ "text": text, "by": "user:ana" }))
            .await;
    }
    k.say(task, "user:ana", "wake").await;
    k.sessions(2).await;
    let lines = thread_lines(&k.host.envelopes()[1]);
    let chars: usize = lines.iter().map(|l| l.chars().count()).sum();
    assert!(chars < 16_000, "{chars}");
    assert_eq!(
        lines.len(),
        12,
        "Ada's reply, the 10 comments, the waking one"
    );
}

// BLO-147: BLO-128 part 2 test pass. The mention routing table (who wakes,
// where, and how often), a mentioned agent's reply on the source thread,
// and a mention storm.

impl Kernel {
    /// `agent`'s mention chat on `task`, if it has one.
    async fn chat_of(&self, task: TaskId, agent: AgentId) -> Option<Task> {
        let mut tx = self.store.read().await.unwrap();
        tx.mention_chat(task, agent).await.unwrap()
    }

    /// An agent task whose first run is over: Needs you, one run. Sessions
    /// finish as soon as they start from here on.
    async fn idle_task(&self, agent: AgentId) -> TaskId {
        self.host.finish.store(true, Ordering::SeqCst);
        let task = self.task(agent, TaskStatus::Todo).await;
        self.until(task, TaskStatus::NeedsYou).await;
        task
    }

    async fn mention(&self, task: TaskId, text: &str, agents: &[AgentId]) {
        self.engine
            .comment(task, text, ana(), agents)
            .await
            .unwrap();
    }
}

/// The routing table on a task Ada (assignee) has finished: who a person's
/// comment wakes for each mention list, and how many runs each gets. Bob,
/// when named, answers from his chat; Ada never gets one.
#[tokio::test(flavor = "multi_thread")]
async fn the_mention_routing_table_wakes_exactly_who_is_named() {
    type Pick = fn(AgentId, AgentId) -> Vec<AgentId>;
    // (row, mentions, Ada's runs on the task, Bob's runs in his chat)
    let rows: [(&str, Pick, usize, Option<usize>); 5] = [
        ("no mentions", |_, _| vec![], 2, None),
        ("[assignee]", |ada, _| vec![ada], 2, None),
        ("[other]", |_, bob| vec![bob], 1, Some(1)),
        ("[assignee, other]", |ada, bob| vec![ada, bob], 2, Some(1)),
        ("[other, assignee]", |ada, bob| vec![bob, ada], 2, Some(1)),
    ];
    for (row, pick, ada_runs, bob_runs) in rows {
        let k = kernel().await;
        let (ada, bob) = (k.agent("Ada").await, k.agent("Bob").await);
        let task = k.idle_task(ada).await;
        k.mention(task, "over to you", &pick(ada, bob)).await;
        eventually(row, || async {
            let bob_now = match k.chat_of(task, bob).await {
                Some(chat) => Some(k.runs(chat.id).await.len()),
                None => None,
            };
            k.runs(task).await.len() == ada_runs && bob_now == bob_runs
        })
        .await;
        settle().await;
        assert_eq!(k.runs(task).await.len(), ada_runs, "{row}: Ada woken once");
        let chat = k.chat_of(task, bob).await;
        let bob_now = match &chat {
            Some(chat) => Some(k.runs(chat.id).await.len()),
            None => None,
        };
        assert_eq!(bob_now, bob_runs, "{row}: Bob");
        assert!(
            k.chat_of(task, ada).await.is_none(),
            "{row}: no chat for the assignee"
        );
        assert!(
            k.host.steers.lock().unwrap().is_empty(),
            "{row}: nobody steered"
        );
        // Every session the row started is accounted for: Ada's first, then one each.
        let started = 1 + (ada_runs - 1) + bob_runs.unwrap_or(0);
        assert_eq!(k.host.envelopes().len(), started, "{row}");
    }
}

/// [other] while the assignee is working: Ada is not steered and keeps her
/// one run; [assignee] while she works steers her, and makes no chat.
#[tokio::test(flavor = "multi_thread")]
async fn naming_the_working_assignee_steers_her_and_naming_another_does_not() {
    let k = kernel().await;
    let (ada, bob) = (k.agent("Ada").await, k.agent("Bob").await);
    let task = k.task(ada, TaskStatus::Todo).await;
    k.working(task).await;
    k.mention(task, "@Bob a second opinion?", &[bob]).await;
    k.sessions(2).await;
    assert!(k.host.steers.lock().unwrap().is_empty());
    k.mention(task, "@Ada use v2", &[ada]).await;
    assert_eq!(*k.host.steers.lock().unwrap(), ["@Ada use v2"]);
    assert_eq!(k.runs(task).await.len(), 1);
    assert!(k.chat_of(task, ada).await.is_none());
}

/// A second mention of Bob: the same chat (no new row), its second run;
/// the chat never shows on the board.
#[tokio::test(flavor = "multi_thread")]
async fn a_second_mention_reuses_the_chat_and_queues_its_next_run() {
    let k = kernel().await;
    let (ada, bob) = (k.agent("Ada").await, k.agent("Bob").await);
    let task = k.idle_task(ada).await;
    k.mention(task, "@Bob first?", &[bob]).await;
    let first = k.chat_of(task, bob).await.unwrap();
    eventually("Bob's first reply", || async {
        k.texts(task).await.len() == 3
    })
    .await;
    k.mention(task, "@Bob second?", &[bob]).await;
    eventually("Bob's second run", || async {
        k.runs(first.id).await.len() == 2
    })
    .await;
    let again = k.chat_of(task, bob).await.unwrap();
    assert_eq!((again.id, again.created_at), (first.id, first.created_at));
    let listed: Vec<TaskId> = k
        .engine
        .tasks()
        .await
        .unwrap()
        .iter()
        .map(|t| t.id)
        .collect();
    assert_eq!(listed, [task]);
    // Another task gets its own chat for Bob.
    let other = k.idle_task(ada).await;
    k.mention(other, "@Bob here too?", &[bob]).await;
    let elsewhere = k.chat_of(other, bob).await.unwrap();
    assert_ne!(elsewhere.id, first.id);
    assert_eq!(elsewhere.source_task_id, Some(other));
    settle().await;
    assert_eq!(k.runs(task).await.len(), 1, "Ada slept through all of it");
}

/// Case 5: Bob's final message is a comment on the source task with exactly
/// `{text, by_agent, run_id}` (his chat's run), his chat's own activity has
/// no comment, and nobody wakes on it. His brief names the task and has no
/// clone instructions. His next fresh session reads the source thread, his
/// reply as "(you)".
#[tokio::test(flavor = "multi_thread")]
async fn a_mentioned_agents_reply_lands_on_the_source_and_wakes_nobody() {
    let k = kernel().await;
    let web = "https://github.com/acme/web.git";
    k.engine
        .add_workspace_repo(WorkspaceId::MAIN, web, None, "", "http://hook", "user:me")
        .await
        .unwrap();
    let (ada, bob) = (k.agent("Ada").await, k.agent("Bob").await);
    let task = k.idle_task(ada).await;
    assert!(k.host.envelopes()[0].contains("## Repositories agents can use"));
    *k.host.claims.lock().unwrap() = vec!["Stable since 2.3.".into()];
    k.mention(task, "@Bob is v2 stable?", &[bob]).await;
    let chat = k.chat_of(task, bob).await.unwrap();
    eventually("Bob's reply", || async { k.texts(task).await.len() == 3 }).await;
    let run = k.runs(chat.id).await[0].id;
    let reply = json!({ "text": "Stable since 2.3.", "by_agent": bob, "run_id": run });
    assert_eq!(k.comments(task).await[2], reply);
    let on_chat = k.engine.activity(chat.id, 0).await.unwrap();
    let kinds: Vec<_> = on_chat.iter().map(|a| a.kind).collect();
    assert!(
        !kinds.contains(&sun_core::model::ActivityKind::Comment),
        "{kinds:?}"
    );
    let brief = k.host.envelopes()[1].clone();
    assert!(
        brief.contains(
            "You are Bob.\n\nYou were mentioned on task MAIN-1 Fix the login. Your final \
             message is posted there as your reply.\n\nIt 500s."
        ),
        "{brief}"
    );
    assert!(!brief.contains("Clone what you need"), "{brief}");
    assert!(!brief.contains("sun/chat-"), "{brief}");
    // Nobody wakes on Bob's reply: not Ada, not Bob again.
    settle().await;
    assert_eq!(k.runs(task).await.len(), 1);
    assert_eq!(k.runs(chat.id).await.len(), 1);
    assert_eq!(k.get(task).await.status, TaskStatus::NeedsYou);

    // Bob again (no native session to resume): his fresh session reads the
    // source thread, his own reply as his.
    k.mention(task, "@Bob and v3?", &[bob]).await;
    k.sessions(3).await;
    let lines = thread_lines(&k.host.envelopes()[2]);
    assert!(
        lines.contains(&"- Bob (you): Stable since 2.3.".to_owned()),
        "{lines:?}"
    );
    assert!(
        lines.contains(&"- Ada: done".to_owned()),
        "Ada's reply under her name: {lines:?}"
    );
}

/// Eight people mention Bob at once on a task he has no chat on yet: one
/// chat, one run, every message reaches it; Ada stays asleep.
#[tokio::test(flavor = "multi_thread")]
async fn a_mention_storm_makes_one_chat_and_one_run() {
    let k = kernel().await;
    let (ada, bob) = (k.agent("Ada").await, k.agent("Bob").await);
    let task = k.idle_task(ada).await;
    k.host.finish.store(false, Ordering::SeqCst);
    let comments: Vec<_> = (0..8)
        .map(|i| {
            let engine = k.engine.clone();
            tokio::spawn(async move {
                engine
                    .comment(task, &format!("@Bob q{i}"), ana(), &[bob])
                    .await
            })
        })
        .collect();
    for comment in comments {
        comment.await.unwrap().unwrap();
    }
    let chat = k.chat_of(task, bob).await.unwrap();
    assert_eq!(k.runs(chat.id).await.len(), 1);
    eventually("every question delivered", || async {
        let envelope = k.host.envelopes().last().cloned().unwrap_or_default();
        let steers = k.host.steers.lock().unwrap().clone();
        (0..8).all(|i| {
            let text = format!("@Bob q{i}");
            envelope.contains(&text) || steers.contains(&text)
        })
    })
    .await;
    assert_eq!(k.runs(task).await.len(), 1);
    assert_eq!(k.comments(task).await.len(), 9, "Ada's reply and the eight");
}

/// An unassigned task: a comment with no mentions wakes nobody, a mention
/// wakes only the named agent, and a later plain comment does not reach
/// that agent's chat.
#[tokio::test(flavor = "multi_thread")]
async fn on_an_unassigned_task_only_a_mention_wakes_anyone() {
    let k = kernel().await;
    let bob = k.agent("Bob").await;
    k.host.finish.store(true, Ordering::SeqCst);
    let new = NewTask {
        workspace_id: Some(WorkspaceId::MAIN),
        title: "Plan the launch".into(),
        body: String::new(),
        workflow: WorkflowPick::None,
        exec_env: None,
        priority: Priority::None,
        status: TaskStatus::Todo,
        project_id: None,
        actor: None,
    };
    let task = k.engine.create_task(new).await.unwrap().id;
    k.mention(task, "just a note", &[]).await;
    k.mention(task, "@Bob dates?", &[bob]).await;
    let chat = k.chat_of(task, bob).await.unwrap();
    eventually("Bob's reply", || async { k.texts(task).await.len() == 3 }).await;
    k.mention(task, "thanks all", &[]).await;
    settle().await;
    assert_eq!(k.runs(chat.id).await.len(), 1);
    assert!(k.runs(task).await.is_empty());
    assert_eq!(k.host.envelopes().len(), 1);
}

/// Case 5, defect probe: a mention chat's session still gets the
/// workspace's "Repositories agents can use" section, from its context pack
/// (`context.rs` lists the workspace's repos for every step), though the
/// chat answers and never clones. Ada's own session on the task, control,
/// does get it.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "BLO-147 defect: context::compile adds the workspace repos section to a mention chat's envelope (crates/sun-engine/src/context.rs:252 does not skip TaskKind::Chat)"]
async fn a_mention_chats_session_gets_no_repository_section() {
    let k = kernel().await;
    let web = "https://github.com/acme/web.git";
    k.engine
        .add_workspace_repo(WorkspaceId::MAIN, web, None, "", "http://hook", "user:me")
        .await
        .unwrap();
    let (ada, bob) = (k.agent("Ada").await, k.agent("Bob").await);
    let task = k.idle_task(ada).await;
    assert!(k.host.envelopes()[0].contains("## Repositories agents can use"));
    k.mention(task, "@Bob is v2 stable?", &[bob]).await;
    k.sessions(2).await;
    let envelope = k.host.envelopes()[1].clone();
    assert!(!envelope.contains("## Repositories"), "{envelope}");
    assert!(!envelope.contains(web), "{envelope}");
}
