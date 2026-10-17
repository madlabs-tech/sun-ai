//! The engine's **Facade** for the API (`sun-api` may not reach `sun-store`):
//! read models in read transactions, and the few writes that are
//! not run transitions (save a workflow version, enrol a host). Task writes
//! are in `tasks`. Run and step transitions stay on the engine task.

use std::collections::HashMap;

use serde::Serialize;
use serde_json::Value;
use sun_core::entitlement::{Entitlement, Quota};
use sun_core::model::{
    Artifact, EventRecord, Host, HostRuntime, Repo, Run, SecretScope, Step, Task, Workflow,
    WorkflowOwner, WorkflowVersion,
};
use sun_core::{
    ArtifactId, BlobHash, Error, HostId, MemberId, Result, RunId, StepKey, TaskId, Timestamp,
    WorkflowId, WorkspaceId,
};
use sun_store::{
    AgentRepo, ArtifactRepo, EventLog, EventQuery, HostRepo, ModInstallRepo, RepoRepo, RunRepo,
    SecretRepo, SettingRepo, TaskRepo, Usage, WorkflowRepo, WorkspaceItem, WorkspaceRepo,
};
use sun_workflow::{StepKind, WorkflowError};
use tokio::sync::broadcast;

use crate::Engine;

/// The steps a run's pinned workflow declares, in file order.
#[derive(Debug, Clone, PartialEq)]
pub struct Plan {
    /// One entry per workflow step.
    pub steps: Vec<PlannedStep>,
    /// Runtime failover chain (`runtime.chain`), empty when not set.
    pub runtime_chain: Vec<String>,
}

/// One workflow step: its key and `type:`.
#[derive(Debug, Clone, PartialEq)]
pub struct PlannedStep {
    /// Step id.
    pub key: StepKey,
    /// `agent`, `gate`, `approval`, `wait`, `notify`, `action` or `extension`.
    pub kind: &'static str,
}

impl Engine {
    /// Every task, highest priority first, then oldest first.
    pub async fn tasks(&self) -> Result<Vec<Task>> {
        Ok(self.store.read().await?.tasks().await?)
    }

    /// One task, or `NotFound`.
    pub async fn task(&self, id: TaskId) -> Result<Task> {
        Ok(self.store.read().await?.get_task(id).await?)
    }

    /// Where the task's sessions work (a worktree, a session dir of worktrees, or a task folder):
    /// what the executor passes to `HostLink::prepare_worktree`.
    pub async fn worktree(&self, id: TaskId) -> Result<sun_core::host_protocol::Worktree> {
        crate::executor::worktree_of(&mut self.store.read().await?, id).await
    }

    /// Every run of a task, oldest first.
    pub async fn runs_of_task(&self, id: TaskId) -> Result<Vec<Run>> {
        Ok(self.store.read().await?.runs_of_task(id).await?)
    }

    /// One run, or `NotFound`.
    pub async fn run(&self, id: RunId) -> Result<Run> {
        Ok(self.store.read().await?.get_run(id).await?)
    }

    /// Every step attempt of a run.
    pub async fn steps(&self, run_id: RunId) -> Result<Vec<Step>> {
        Ok(self.store.read().await?.steps_of_run(run_id).await?)
    }

    /// The steps and runtime chain of the run's pinned workflow version.
    pub async fn plan(&self, run: &Run) -> Result<Plan> {
        let version = self
            .store
            .read()
            .await?
            .get_workflow_version(run.workflow_id, run.workflow_version)
            .await?;
        let workflow = parse(&version.source_yaml)?;
        Ok(Plan {
            steps: workflow
                .steps
                .iter()
                .map(|step| PlannedStep {
                    key: step.id.clone(),
                    kind: kind_name(&step.kind),
                })
                .collect(),
            runtime_chain: workflow
                .runtime
                .map(|r| sun_core::chain_names(&r.chain))
                .unwrap_or_default(),
        })
    }

    /// Up to `limit` events of the run with `seq > after`, ascending.
    pub async fn events(&self, run_id: RunId, after: i64, limit: u32) -> Result<Vec<EventRecord>> {
        Ok(self
            .store
            .read()
            .await?
            .events_after(Some(run_id), after, limit)
            .await?)
    }

    /// The run's events with `seq > after` of `kind` and containing `text`
    /// (either filter optional), ascending: the timeline search.
    pub async fn search_events(
        &self,
        run_id: RunId,
        kind: Option<&str>,
        text: Option<&str>,
        after: i64,
        limit: u32,
    ) -> Result<Vec<EventRecord>> {
        Ok(self
            .store
            .read()
            .await?
            .search_events(run_id, kind, text, after, limit)
            .await?)
    }

    /// Every event committed from now on (**Observer**). Subscribe before
    /// paging [`events`](Self::events) so nothing falls in between; on
    /// `Lagged`, page again from the last `seq` seen.
    pub fn subscribe(&self) -> broadcast::Receiver<EventRecord> {
        self.store.subscribe()
    }

    /// Every workflow with its latest version number (0 if none saved).
    pub async fn workflows(&self) -> Result<Vec<(Workflow, u32)>> {
        let mut tx = self.store.read().await?;
        let mut out = Vec::new();
        for workflow in tx.workflows().await? {
            let version = tx
                .latest_workflow_version(workflow.id)
                .await?
                .map_or(0, |v| v.version);
            out.push((workflow, version));
        }
        Ok(out)
    }

    /// Skill and MCP server names the saved workflows of `workspace` (`None`:
    /// every workspace's) use (agent steps' `skills:` and every `mcp:`
    /// grant), sorted: catalog ranking.
    // ponytail: one read and parse per workflow; cache by version if lists grow.
    pub async fn catalog_names_in_use(
        &self,
        workspace: Option<WorkspaceId>,
    ) -> Result<Vec<String>> {
        let mut names = std::collections::BTreeSet::new();
        for (workflow, version) in self.workflows().await? {
            if version == 0 || workspace.is_some_and(|w| w != workflow.workspace_id) {
                continue;
            }
            let (_, saved) = self.workflow(workflow.id).await?;
            // Saved versions passed validation; one that no longer parses
            // (an older schema) just doesn't rank anything.
            let Ok(parsed) = sun_workflow::parse(&saved.source_yaml) else {
                continue;
            };
            names.extend(parsed.mcp.iter().map(|g| g.server.clone()));
            for step in &parsed.steps {
                if let StepKind::Agent { skills, mcp, .. } = &step.kind {
                    names.extend(skills.iter().cloned());
                    names.extend(mcp.iter().map(|g| g.server.clone()));
                }
            }
        }
        Ok(names.into_iter().collect())
    }

    /// A workflow with its latest saved version (the builder edits this);
    /// a hidden one answers `NotFound`.
    pub async fn workflow(&self, id: WorkflowId) -> Result<(Workflow, WorkflowVersion)> {
        let mut tx = self.store.read().await?;
        let workflow = tx.get_workflow(id).await?;
        if tx.workflow_owner(id).await? != WorkflowOwner::Person {
            return Err(Error::NotFound(format!("workflow {id}")));
        }
        let version = tx
            .latest_workflow_version(id)
            .await?
            .ok_or_else(|| Error::NotFound(format!("a saved version of workflow {id}")))?;
        Ok((workflow, version))
    }

    /// Validates `source_yaml` and saves it as the next version of workflow
    /// `id`, creating the workflow on workspace Main when `id` is
    /// `None`. A run keeps the version it was pinned to.
    pub async fn save_workflow(
        &self,
        id: Option<WorkflowId>,
        source_yaml: &str,
    ) -> Result<(Workflow, WorkflowVersion)> {
        self.save_workflow_on(WorkspaceId::MAIN, id, source_yaml)
            .await
    }

    /// [`Self::save_workflow`], creating the workflow on `workspace_id` when `id`
    /// is `None` (`NotFound` if the workspace doesn't exist). An existing workflow
    /// stays on its own workspace.
    pub async fn save_workflow_on(
        &self,
        workspace_id: WorkspaceId,
        id: Option<WorkflowId>,
        source_yaml: &str,
    ) -> Result<(Workflow, WorkflowVersion)> {
        let parsed = parse(source_yaml)?;
        let mut tx = self.store.begin().await?;
        let workflow = match id {
            Some(id) => {
                let mut workflow = tx.get_workflow(id).await?;
                // Only the engine saves a hidden workflow.
                if tx.workflow_owner(id).await? != WorkflowOwner::Person {
                    return Err(Error::NotFound(format!("workflow {id}")));
                }
                if workflow.name != parsed.name {
                    tx.rename_workflow(id, &parsed.name).await?;
                    workflow.name = parsed.name.clone();
                }
                workflow
            }
            None => {
                tx.get_workspace(workspace_id).await?;
                let workflow = Workflow {
                    id: WorkflowId::generate(),
                    workspace_id,
                    name: parsed.name.clone(),
                };
                tx.insert_workflow(&workflow).await?;
                workflow
            }
        };
        if let Some(runtime) = &parsed.runtime {
            let workspace = workflow.workspace_id;
            crate::chain_check::check(&mut tx, workspace, &runtime.chain, "runtime.chain").await?;
        }
        let version = tx
            .save_workflow_version(workflow.id, source_yaml, &parsed.hash)
            .await?;
        tx.commit().await?;
        // Its mod triggers start tasks from now on.
        for t in parsed
            .on
            .iter()
            .filter_map(|t| crate::triggers::ModTrigger::parse(t))
        {
            if let Err(e) = self
                .mods
                .subscribe(&t.mod_name, vec![t.topic.clone()])
                .await
            {
                eprintln!(
                    "sun: workflow {}: trigger {}.{}: {e}",
                    parsed.name, t.mod_name, t.topic
                );
            }
        }
        Ok((workflow, version))
    }

    /// Summed token and cost usage, of one run or of all.
    pub async fn usage(&self, run_id: Option<RunId>) -> Result<Usage> {
        Ok(self.store.read().await?.usage(run_id).await?)
    }

    /// Summed usage per runtime name, since `since` when given.
    pub async fn usage_by_runtime(
        &self,
        since: Option<sun_core::Timestamp>,
    ) -> Result<Vec<(String, Usage)>> {
        Ok(self
            .store
            .read()
            .await?
            .usage_by_runtime(since.as_ref())
            .await?)
    }

    /// Enrols a new host named `name`, attached to `workspace` (`None`: to
    /// none, a leased seller host), with who minted its token. `PlanLimit`
    /// when the workspace's plan has no room for another host.
    pub async fn add_host(
        &self,
        name: String,
        workspace: Option<(WorkspaceId, &Entitlement)>,
        enrolled_by: Option<MemberId>,
    ) -> Result<Host> {
        let host = Host {
            id: HostId::generate(),
            name,
            capabilities: Value::Object(Default::default()),
            enrolled_by,
        };
        let mut tx = self.store.begin().await?;
        tx.insert_host(&host).await?;
        if let Some((workspace, plan)) = workspace {
            plan.check(Quota::Hosts, tx.hosts_in(workspace).await?.len())?;
            tx.link(workspace, WorkspaceItem::Host(host.id)).await?;
        }
        tx.commit().await?;
        Ok(host)
    }

    /// Saves a host enrolment token (by its hash) with what it `enrols`.
    pub async fn add_host_token(
        &self,
        token_hash: &str,
        enrols: &Value,
        expires_at: Timestamp,
    ) -> Result<()> {
        let mut tx = self.store.begin().await?;
        tx.insert_host_token(token_hash, enrols, expires_at).await?;
        Ok(tx.commit().await?)
    }

    /// Spends a host enrolment token: what it enrols and when it expires,
    /// or `None` when it is unknown or already used.
    pub async fn take_host_token(&self, token_hash: &str) -> Result<Option<(Value, Timestamp)>> {
        let mut tx = self.store.begin().await?;
        let found = tx.take_host_token(token_hash).await?;
        tx.commit().await?;
        Ok(found)
    }

    /// One host, or `NotFound`.
    pub async fn host(&self, id: HostId) -> Result<Host> {
        Ok(self.store.read().await?.get_host(id).await?)
    }

    /// The hosts attached to `workspace` (`None`: every enrolled host).
    pub async fn hosts(&self, workspace: Option<WorkspaceId>) -> Result<Vec<Host>> {
        let mut read = self.store.read().await?;
        Ok(match workspace {
            Some(workspace) => read.hosts_in(workspace).await?,
            None => read.hosts().await?,
        })
    }

    /// The `(host, tool)` rows of the hosts attached to `workspace`
    /// (`None`: every host) that their latest report names.
    pub async fn host_runtimes(&self, workspace: Option<WorkspaceId>) -> Result<Vec<HostRuntime>> {
        Ok(self.store.read().await?.host_runtimes(workspace).await?)
    }

    /// The workspaces `item` is linked to (a host's, a catalog entry's).
    pub async fn workspaces_of(&self, item: WorkspaceItem<'_>) -> Result<Vec<WorkspaceId>> {
        Ok(self.store.read().await?.workspaces_of(item).await?)
    }

    /// The catalog ids `workspace` installed, sorted.
    pub async fn catalog_installs(&self, workspace: WorkspaceId) -> Result<Vec<String>> {
        Ok(self.store.read().await?.catalog_installs(workspace).await?)
    }

    /// Stores what a host reported in its `hello`.
    pub async fn set_host_capabilities(&self, id: HostId, capabilities: &Value) -> Result<()> {
        let mut tx = self.store.begin().await?;
        tx.set_host_capabilities(id, capabilities).await?;
        Ok(tx.commit().await?)
    }

    /// Marks `key` used until `expires_at`: `false` when it already was (a
    /// replayed webhook delivery).
    pub async fn use_once(&self, key: &str, expires_at: Timestamp) -> Result<bool> {
        let mut tx = self.store.begin().await?;
        let first = tx.use_once(key, expires_at).await?;
        tx.commit().await?;
        Ok(first)
    }

    /// Frees `key` after its use failed (a webhook delivery the mod failed
    /// on), so a redelivery is handled.
    pub async fn release_once(&self, key: &str) -> Result<()> {
        let mut tx = self.store.begin().await?;
        tx.release_once(key).await?;
        Ok(tx.commit().await?)
    }

    /// The repos of `workspace`, oldest first (`NotFound` if missing).
    pub async fn repos(&self, workspace: WorkspaceId) -> Result<Vec<Repo>> {
        let mut tx = self.store.read().await?;
        tx.get_workspace(workspace).await?;
        Ok(tx.repos(workspace).await?)
    }

    /// A task's artifacts, oldest version first.
    pub async fn artifacts(&self, task_id: TaskId) -> Result<Vec<Artifact>> {
        Ok(self.store.read().await?.artifacts_of(task_id).await?)
    }

    /// Secret `name` of mod `mod_name` (e.g. `WEBHOOK_SECRET`), or `None`.
    pub async fn mod_secret(&self, mod_name: &str, name: &str) -> Result<Option<String>> {
        let scope = SecretScope::Mod(mod_name.to_owned());
        match self.store.read().await?.secret_value(&scope, name).await {
            Ok(secret) => Ok(Some(secret)),
            Err(e) => match Error::from(e) {
                Error::NotFound(_) => Ok(None),
                e => Err(e),
            },
        }
    }

    /// One artifact and its content.
    pub async fn artifact(&self, id: ArtifactId) -> Result<(Artifact, Vec<u8>)> {
        let artifact = self.store.read().await?.get_artifact(id).await?;
        let bytes = self.store.blobs().get(&artifact.blob).await?;
        Ok((artifact, bytes))
    }

    /// A device or canvas screenshot of run `run_id`: only a blob
    /// one of the run's `device_result` or `canvas_screens` events cites, so a
    /// run id grants no other blob.
    pub async fn device_screenshot(&self, run_id: RunId, blob: &BlobHash) -> Result<Vec<u8>> {
        let events = self
            .store
            .read()
            .await?
            .events_after(Some(run_id), 0, u32::MAX)
            .await?;
        let cited = events
            .iter()
            .filter(|e| e.kind == "device_result" || e.kind == "canvas_screens")
            .any(|e| {
                e.payload["screenshots"]
                    .as_array()
                    .is_some_and(|shots| shots.iter().any(|s| s["blob"] == blob.as_str()))
            });
        if !cited {
            return Err(Error::NotFound(format!(
                "screenshot {} of run {run_id}",
                blob.as_str()
            )));
        }
        Ok(self.store.blobs().get(blob).await?)
    }
}

/// A workflow's dry run: what would run, in which order, what it
/// needs and what it may cost. Nothing is saved or started.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct DryRun {
    /// Every step in file order.
    pub steps: Vec<DryStep>,
    /// `runtime.chain`; empty means the ecosystem default runtime.
    pub runtimes: Vec<String>,
    /// The plugins and extensions it needs, and whether each is installed.
    pub mods: Vec<ModNeed>,
    /// Agent steps (each is at least one paid agent session).
    pub agent_steps: u32,
    /// Agent steps × the average cost of past agent sessions; `None` before
    /// any session reported usage.
    pub estimated_cost_usd: Option<f64>,
    /// The workflow's own cap, `budget.cost_usd`.
    pub budget_usd: Option<f64>,
    /// The steps actually run in a sandbox with the scripted fake runtime,
    /// no side effects (outbox actions recorded, not sent).
    pub simulation: crate::Simulation,
}

/// One step of a [`DryRun`].
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct DryStep {
    /// Step id.
    pub id: StepKey,
    /// Its `type:` family, as in [`PlannedStep::kind`].
    pub kind: &'static str,
    /// 1-based order: a step runs once every step of earlier waves it needs
    /// succeeded; steps of one wave may run at once.
    pub wave: u32,
    /// What it uses: the action, extension, gates, approvers or targets.
    pub uses: Vec<String>,
}

/// A mod a workflow needs.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ModNeed {
    /// Mod name.
    pub name: String,
    /// Installed on this kernel.
    pub installed: bool,
}

impl Engine {
    /// Dry-runs `source_yaml`; `InvalidInput` when it doesn't parse.
    pub async fn dry_run(&self, source_yaml: &str) -> Result<DryRun> {
        let workflow = parse(source_yaml)?;
        let mut tx = self.store.read().await?;
        let installed: Vec<String> = tx
            .mod_installs()
            .await?
            .into_iter()
            .map(|m| m.name)
            .collect();
        let sessions = tx.count_events("session_started").await?;
        let spent = tx.usage(None).await?.cost_usd;
        let mut waves: HashMap<&StepKey, u32> = HashMap::new();
        // Longest `needs` path; at most one pass per step (no cycles parse).
        for _ in 0..workflow.steps.len() {
            for step in &workflow.steps {
                let wave = 1 + step
                    .needs
                    .iter()
                    .map(|n| waves.get(n).copied().unwrap_or(0))
                    .max()
                    .unwrap_or(0);
                waves.insert(&step.id, wave);
            }
        }
        let steps: Vec<DryStep> = workflow
            .steps
            .iter()
            .map(|step| DryStep {
                id: step.id.clone(),
                kind: kind_name(&step.kind),
                wave: waves[&step.id],
                uses: uses(&step.kind),
            })
            .collect();
        let agent_steps =
            u32::try_from(steps.iter().filter(|s| s.kind == "agent").count()).unwrap_or(u32::MAX);
        #[allow(clippy::cast_precision_loss)] // a session count far below 2^52
        let per_session = (sessions > 0 && spent > 0.0).then(|| spent / sessions as f64);
        drop(tx);
        let simulation = self.simulate(source_yaml, &workflow).await?;
        Ok(DryRun {
            simulation,
            mods: workflow
                .mods()
                .into_iter()
                .map(|name| ModNeed {
                    installed: installed.contains(&name),
                    name,
                })
                .collect(),
            runtimes: workflow
                .runtime
                .map(|r| sun_core::chain_names(&r.chain))
                .unwrap_or_default(),
            estimated_cost_usd: per_session.map(|c| c * f64::from(agent_steps)),
            budget_usd: workflow.budget.cost_usd,
            agent_steps,
            steps,
        })
    }
}

/// What a step uses, for people reading a dry run.
fn uses(kind: &StepKind) -> Vec<String> {
    match kind {
        StepKind::Agent { skills, .. } => skills.clone(),
        StepKind::Gate { gates, .. } => gates.iter().map(|g| g.name.clone()).collect(),
        StepKind::Approval { who, .. } | StepKind::Decide { who, .. } => who.clone(),
        StepKind::Notify { to, .. } => to.clone(),
        StepKind::Action { target: name, .. } | StepKind::Extension { name, .. } => {
            vec![name.clone()]
        }
        StepKind::Wait { .. } => Vec::new(),
    }
}

/// Validates a workflow file without saving it: every problem with its
/// `line:column`, empty when the file is valid (the builder's live check).
pub fn check_workflow(source_yaml: &str) -> Vec<WorkflowError> {
    sun_workflow::parse(source_yaml).err().unwrap_or_default()
}

/// Whether an agent step of `source_yaml` sets `permission_mode:
/// bypass_permissions` (`false` when it doesn't parse).
pub fn sets_bypass(source_yaml: &str) -> bool {
    sun_workflow::parse(source_yaml).is_ok_and(|w| {
        w.steps
            .iter()
            .any(|s| w.permission_mode(&s.id) == Some(sun_core::PermissionMode::BypassPermissions))
    })
}

pub(crate) fn parse(source_yaml: &str) -> Result<sun_workflow::Workflow> {
    sun_workflow::parse(source_yaml).map_err(|errors| {
        let errors: Vec<String> = errors.iter().map(ToString::to_string).collect();
        Error::InvalidInput(errors.join("; "))
    })
}

fn kind_name(kind: &StepKind) -> &'static str {
    match kind {
        StepKind::Agent { .. } => "agent",
        StepKind::Gate { .. } => "gate",
        StepKind::Approval { .. } => "approval",
        StepKind::Wait { .. } => "wait",
        StepKind::Notify { .. } => "notify",
        StepKind::Action { .. } => "action",
        StepKind::Decide { .. } => "decide",
        StepKind::Extension { .. } => "extension",
    }
}
