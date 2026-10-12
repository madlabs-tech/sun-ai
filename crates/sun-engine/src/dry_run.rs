//! Builder dry run, simulated: the workflow's steps actually run,
//! on the eval replay sandbox — a throwaway store and a second [`Engine`] —
//! with no side effects:
//! - every agent session is the **scripted fake runtime** in-kernel (a short
//!   transcript that claims done and exits 0), so no model runs;
//! - the host is [`DryHost`]: nothing is cloned, written or executed, and
//!   every command (gates, the push) reports success;
//! - mods go through the eval `Recorder`: outbox actions are recorded, never
//!   delivered; approvals are approved automatically.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde::Serialize;
use serde_json::json;
use sun_core::host_protocol::{ExecOutput, ExecRequest, SessionFile, Worktree};
use sun_core::model::{Host, Run, Task};
use sun_core::{
    BoxStream, Error, HostEvent, HostId, HostLink, Result, Role, RunId, RunStatus, SessionHandle,
    SessionSpec, Steer, StepKey, StopMode, SunEvent, TaskId, TaskStatus, WorkflowId,
};
use sun_store::{EventLog, HostRepo, NewEvent, RunRepo, Store, TaskRepo, WorkflowRepo};
use sun_workflow::{StepKind, Workflow};

use crate::eval::{REPLAY, Recorder, Sandbox};
use crate::{Engine, EngineConfig, WatchdogConfig};

/// A simulation still going after this is cancelled (e.g. a long `wait`).
const SIMULATION_TIMEOUT: Duration = Duration::from_secs(20);

/// What happened to one step in the simulation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SimStep {
    /// Step id.
    pub id: StepKey,
    /// Its last attempt's status (`succeeded`, `failed`, …), or
    /// `not_reached` when the simulation ended before it.
    pub status: String,
    /// Attempts it took.
    pub attempts: u32,
    /// How the simulation stood in for the real thing.
    pub note: Option<String>,
}

/// The simulated run of a dry run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Simulation {
    /// How the run ended (`succeeded`, `failed`, `cancelled`).
    pub status: String,
    /// Why, when it didn't succeed.
    pub end_reason: Option<String>,
    /// Every workflow step, in file order.
    pub steps: Vec<SimStep>,
    /// Outbox actions the run would send, recorded and not sent (`github.open_pr`).
    pub actions: Vec<String>,
}

impl Engine {
    /// Runs `workflow` (parsed from `yaml`) in a throwaway sandbox with the
    /// scripted fake runtime and no side effects.
    pub(crate) async fn simulate(&self, yaml: &str, workflow: &Workflow) -> Result<Simulation> {
        let dir = std::env::temp_dir().join(format!("sun-dry-run-{}", TaskId::generate()));
        let result = self.simulate_in(&dir, yaml, workflow).await;
        let _ = std::fs::remove_dir_all(&dir);
        result
    }

    async fn simulate_in(
        &self,
        dir: &std::path::Path,
        yaml: &str,
        workflow: &Workflow,
    ) -> Result<Simulation> {
        let sandbox = Store::open(dir).await?;
        let row = sun_core::model::Workflow {
            id: WorkflowId::generate(),
            workspace_id: sun_core::WorkspaceId::MAIN,
            name: workflow.name.clone(),
        };
        let task = Task {
            title: format!("Dry run of {}", workflow.name),
            workflow_id: Some(row.id),
            status: TaskStatus::Todo,
            ..sun_core::model::Task::new(sun_core::WorkspaceId::MAIN, "")
        };
        let run = Run {
            id: RunId::generate(),
            task_id: task.id,
            workflow_id: row.id,
            workflow_version: 1,
            status: RunStatus::Queued,
            end_reason: None,
            budget: workflow.budget.clone(),
            repo_ids: Vec::new(),
        };
        let mut tx = sandbox.begin().await?;
        tx.insert_workflow(&row).await?;
        tx.save_workflow_version(row.id, yaml, &workflow.hash)
            .await?;
        tx.insert_host(&Host {
            id: self.host,
            name: "dry-run".into(),
            capabilities: json!({}),
            enrolled_by: None,
        })
        .await?;
        tx.insert_task(&task).await?;
        tx.insert_run(&run).await?;
        tx.append_event(NewEvent::new(
            run.id,
            None,
            "run_status",
            json!({ "from": null, "to": RunStatus::Queued.as_str(), "reason": null }),
        ))
        .await?;
        tx.commit().await?;

        let recorder = Arc::new(Recorder::default());
        let link = Arc::new(Sandbox::new(
            Arc::new(DryHost(dir.join("work"))),
            scripts(workflow),
            true,
        ));
        let config = EngineConfig {
            host: self.host,
            adapter: REPLAY.into(),
            delivery_timeout: Duration::from_secs(5),
            watchdog: WatchdogConfig::default(),
        };
        let (engine, engine_task) =
            Engine::start(sandbox.clone(), link, recorder.clone(), config).await?;
        let ended = engine
            .drive(run.id, workflow, SIMULATION_TIMEOUT, Default::default())
            .await;
        engine_task.abort();
        let ended = ended?;
        let mut attempts: HashMap<StepKey, (u32, String)> = HashMap::new();
        for step in sandbox.read().await?.steps_of_run(run.id).await? {
            let entry = attempts.entry(step.step_key).or_default();
            if step.attempt >= entry.0 {
                *entry = (step.attempt, step.status.as_str().to_owned());
            }
        }
        let steps = workflow
            .steps
            .iter()
            .map(|step| {
                let (attempts, status) = attempts
                    .remove(&step.id)
                    .unwrap_or((0, "not_reached".into()));
                SimStep {
                    id: step.id.clone(),
                    status,
                    attempts,
                    note: note(&step.kind),
                }
            })
            .collect();
        let actions = recorder.calls.lock().map(|c| c.clone()).unwrap_or_default();
        Ok(Simulation {
            status: ended.status.as_str().to_owned(),
            end_reason: ended.end_reason,
            steps,
            actions,
        })
    }
}

/// The fake runtime's script for every agent step: it reports and claims done.
fn scripts(workflow: &Workflow) -> HashMap<StepKey, Vec<SunEvent>> {
    workflow
        .steps
        .iter()
        .filter(|s| matches!(s.kind, StepKind::Agent { .. }))
        .map(|s| {
            let script = vec![
                SunEvent::Text {
                    role: Role::Assistant,
                    text: format!("Dry run: `{}` simulated, no model ran.", s.id),
                },
                SunEvent::DoneClaimed {
                    summary: "simulated".into(),
                },
                SunEvent::Exited { code: Some(0) },
            ];
            (s.id.clone(), script)
        })
        .collect()
}

fn note(kind: &StepKind) -> Option<String> {
    Some(
        match kind {
            StepKind::Agent { .. } => "Scripted fake runtime; no model ran",
            StepKind::Gate { .. } => "Commands not run; reported as passing",
            StepKind::Approval { .. } | StepKind::Decide { .. } => {
                "Approved automatically; a person decides in a real run"
            }
            _ => return None,
        }
        .into(),
    )
}

struct NoEvents;

impl futures_core::Stream for NoEvents {
    type Item = HostEvent;

    fn poll_next(
        self: std::pin::Pin<&mut Self>,
        _: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<HostEvent>> {
        std::task::Poll::Ready(None)
    }
}

/// A host that does nothing: no worktree is cloned, no file written, no
/// command run (each reports success), no session started (the sandbox
/// replays every one).
struct DryHost(PathBuf);

#[async_trait]
impl HostLink for DryHost {
    async fn start_session(&self, _: HostId, _: &str, _: SessionSpec) -> Result<SessionHandle> {
        Err(Error::Unavailable("dry run: sessions are simulated".into()))
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
        // No real sessions: the sandbox's replays are its only events.
        Box::pin(NoEvents)
    }
    async fn ack(&self, _: sun_core::SessionId, _: u64) -> Result<()> {
        Ok(())
    }
    fn is_online(&self, _: HostId) -> bool {
        true
    }
    async fn prepare_worktree(&self, _: HostId, _: Worktree) -> Result<PathBuf> {
        Ok(self.0.clone())
    }
    async fn remove_worktree(&self, _: HostId, _: TaskId) -> Result<()> {
        Ok(())
    }
    async fn write_files(&self, _: HostId, _: PathBuf, _: Vec<SessionFile>) -> Result<()> {
        Ok(())
    }
    async fn exec(&self, _: HostId, _: ExecRequest) -> Result<ExecOutput> {
        Ok(ExecOutput {
            code: Some(0),
            output: "dry run: not run".into(),
            timed_out: false,
        })
    }
}
