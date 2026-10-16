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
        _: HostId,
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
        self.adapters.lock().unwrap().push(adapter.to_owned());
        self.skills.lock().unwrap().push(spec.skills.clone());
        self.envelopes.lock().unwrap().push(spec.envelope);
        self.alive.lock().unwrap().insert(id);
        let (finish, alive, sink) = (self.finish.clone(), self.alive.clone(), self.sink.clone());
        tokio::spawn(async move {
            let send = |seq, event| {
                if let Some(sink) = sink.lock().unwrap().as_ref() {
                    let _ = sink.send(HostEvent {
                        session_id: id,
                        seq,
                        event,
                    });
                }
            };
            tokio::time::sleep(Duration::from_millis(5)).await;
            let working = SunEvent::Text {
                role: Role::Assistant,
                text: "working".into(),
            };
            send(1, working);
            while !finish.load(Ordering::SeqCst) {
                if !alive.lock().unwrap().contains(&id) {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            if !alive.lock().unwrap().remove(&id) {
                return;
            }
            let done = SunEvent::DoneClaimed {
                summary: "done".into(),
            };
            send(2, done);
            send(3, SunEvent::Exited { code: Some(0) });
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
    /// The one enrolled host, attached to Main.
    host_id: HostId,
    _dir: tempfile::TempDir,
}

async fn kernel() -> Kernel {
    kernel_with(Caps::default()).await
}

async fn kernel_with(caps: Caps) -> Kernel {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).await.unwrap();
    let host_id = HostId::generate();
    let mut tx = store.begin().await.unwrap();
    let row = Host {
        id: host_id,
        name: "box".into(),
        capabilities: json!({}),
        enrolled_by: None,
    };
    tx.insert_host(&row).await.unwrap();
    tx.link(WorkspaceId::MAIN, WorkspaceItem::Host(host_id))
        .await
        .unwrap();
    tx.commit().await.unwrap();
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
    let k = kernel_with(one_at_a_time).await;
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
