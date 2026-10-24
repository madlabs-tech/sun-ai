//! Workspaces, part of the **Facade** for the API. A workspace
//! owns its workflows, projects, tasks (`task.workspace_id`,
//! [`Engine::workspace_of_task`]), repos (`vcs`), agents, runtimes, secrets
//! and memory. Knowledge sources are
//! linked to workspaces; an admin imports one into another workspace as the
//! same row ([`Engine::import_to_workspace`]). Each change commits with its
//! audit row.
//!
//! Access: a workspace is visible to its members (`workspace_member`, its
//! creator is its first owner) and to deployment owners
//! ([`crate::Access::workspaces`]). [`Engine::reach`] says which workspaces anything
//! workspace-scoped lives on, for the API's one access guard.

use sun_core::entitlement::{Entitlement, Quota};
use sun_core::model::{
    AuditAction, GitSettings, GitSettingsPatch, OutboxStatus, Role, SignupPolicy, Task, Workflow,
    WorkflowOwner, Workspace,
};
use sun_core::{
    AgentId, ArtifactId, CorrectionId, Error, EvalRunId, EvalSetId, HostId, MemberId, MemoryItemId,
    NoteId, OutboxId, ProjectId, RepoId, Result, RunId, TaskId, VcsConnectionId, WorkflowId,
    WorkspaceId,
};
use sun_store::{
    AgentRepo, ArtifactRepo, AuditRepo, CorrectionRepo, EvalRepo, HostRepo, KnowledgeRepo,
    MemberRepo, MembershipRepo, MemoryRepo, NoteRepo, OutboxRepo, ProjectRepo, RepoRepo, RunRepo,
    TaskRepo, Tx, VcsRepo, WorkflowRepo, WorkspaceItem, WorkspaceRepo,
};

use crate::{Engine, Pick};

/// `PATCH /workspaces/{id}`: the profile fields to change; absent ones stay.
#[derive(Debug, Clone, Default, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProfileChange {
    /// New name (1–64 characters, unique ignoring case).
    pub name: Option<String>,
    /// New description.
    pub description: Option<String>,
    /// New agent context, for every run's context pack.
    pub context: Option<String>,
    /// `Some(None)` (JSON `null`) clears the default workflow.
    #[serde(default, deserialize_with = "present")]
    pub default_workflow_id: Option<Option<WorkflowId>>,
    /// The git switches to change; the others keep their value.
    pub git_settings: Option<GitSettingsPatch>,
}

/// A present field, even `null`, is `Some`; an absent one stays `None`.
pub(crate) fn present<'de, D, T>(deserializer: D) -> std::result::Result<Option<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: serde::Deserialize<'de>,
{
    T::deserialize(deserializer).map(Some)
}

/// Something that lives on workspaces, as the access guard finds it in a path,
/// query or body.
#[derive(Debug, Clone, Copy)]
pub enum OnWorkspace<'a> {
    /// A workspace.
    Workspace(WorkspaceId),
    /// A task: its workspace.
    Task(TaskId),
    /// A run: its task's workspace.
    Run(RunId),
    /// A workflow.
    Workflow(WorkflowId),
    /// A repo: its workspace.
    Repo(RepoId),
    /// A knowledge source: any of its workspaces.
    Item(WorkspaceItem<'a>),
    /// A knowledge doc: its source's workspaces.
    Doc(&'a str),
    /// An artifact: its task's workspace.
    Artifact(ArtifactId),
    /// An eval: every workspace its picked tasks are on.
    Eval(EvalRunId),
    /// An eval set: every workspace its tasks are on.
    EvalSet(EvalSetId),
    /// An eval's pick: every workspace it can replay from.
    Pick(&'a Pick),
    /// An `unknown` side effect waiting for a human: its run's workspace; one
    /// outside any run is on no workspace, so everyone sees it.
    Outbox(OutboxId),
    /// A project.
    Project(ProjectId),
    /// A git connection: its workspace.
    VcsConnection(VcsConnectionId),
    /// An agent: its workspace.
    Agent(AgentId),
    /// A memory item: its workspace.
    MemoryItem(MemoryItemId),
    /// A correction: its task's workspace.
    Correction(CorrectionId),
    /// A note: its run's workspace.
    Note(NoteId),
}

/// Which workspaces something lives on ([`Engine::reach`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reach {
    /// Visible when any of these is (a shared repo or source; one workspace).
    Any(Vec<WorkspaceId>),
    /// Visible only when every one of these is (an eval over many tasks).
    All(Vec<WorkspaceId>),
}

impl Reach {
    /// True when `sees` admits it.
    pub fn visible(&self, sees: impl Fn(WorkspaceId) -> bool) -> bool {
        match self {
            Reach::Any(workspaces) => workspaces.iter().any(|b| sees(*b)),
            Reach::All(workspaces) => workspaces.iter().all(|b| sees(*b)),
        }
    }
}

async fn workspace_of_task(tx: &mut Tx, id: TaskId) -> Result<WorkspaceId> {
    Ok(tx.get_task(id).await?.workspace_id)
}

/// The standing instructions of the task's workspace, for its context packs.
pub(crate) async fn workspace_context(tx: &mut Tx, task: TaskId) -> Result<String> {
    let workspace = workspace_of_task(tx, task).await?;
    Ok(tx.get_workspace(workspace).await?.context)
}

async fn workspaces_of_tasks(tx: &mut Tx, ids: &[TaskId]) -> Result<Vec<WorkspaceId>> {
    let mut workspaces = Vec::new();
    for id in ids {
        workspaces.push(workspace_of_task(tx, *id).await?);
    }
    Ok(workspaces)
}

async fn all_workspaces(tx: &mut Tx) -> Result<Vec<WorkspaceId>> {
    Ok(tx.workspaces().await?.into_iter().map(|b| b.id).collect())
}

async fn reach_of_pick(tx: &mut Tx, pick: &Pick) -> Result<Reach> {
    let workspaces = match pick {
        Pick::Workflow { workflow_id } => vec![tx.get_workflow(*workflow_id).await?.workspace_id],
        Pick::Tasks { task_ids } => workspaces_of_tasks(tx, task_ids).await?,
        Pick::Set { set_id } => {
            let set = tx.get_eval_set(*set_id).await?;
            workspaces_of_tasks(tx, &set.task_ids).await?
        }
        // Corrections from every workspace.
        Pick::Cause { .. } => all_workspaces(tx).await?,
    };
    Ok(Reach::All(workspaces))
}

impl Engine {
    /// Every workspace, oldest first (Main first).
    pub async fn workspaces(&self) -> Result<Vec<Workspace>> {
        Ok(self.store.read().await?.workspaces().await?)
    }

    /// One workspace, or `NotFound`.
    pub async fn workspace(&self, id: WorkspaceId) -> Result<Workspace> {
        Ok(self.store.read().await?.get_workspace(id).await?)
    }

    /// Creates a workspace named `name` (trimmed, 1–64 chars, unique ignoring
    /// case) and makes `creator` its owner, in one transaction (`None`: the
    /// token holder, a deployment owner who sees every workspace anyway).
    /// `slug` defaults from the name (made unique with `-2`, …); a given one
    /// must be free. The issue prefix comes from the slug, made unique in
    /// the ecosystem with a digit (`MAIN` → `MAIN2`), as the task migration
    /// does. With `policy.disable_workspace_creation`, only deployment
    /// owners may (`Forbidden`).
    pub async fn create_workspace(
        &self,
        name: &str,
        slug: Option<&str>,
        creator: Option<MemberId>,
        policy: &SignupPolicy,
        actor: &str,
    ) -> Result<Workspace> {
        let name = Workspace::clean_name(name)?;
        let slug = slug.map(Workspace::clean_slug).transpose()?;
        let mut tx = self.store.begin().await?;
        if let Some(creator) = creator
            && policy.disable_workspace_creation
            && !tx.get_member(creator).await?.is_owner
        {
            return Err(Error::Forbidden(
                "Only the deployment's owners can create workspaces here.".into(),
            ));
        }
        let slug = match slug {
            Some(slug) => slug,
            None => tx.free_slug(&Workspace::slug_from_name(&name)).await?,
        };
        let issue_prefix = tx.free_prefix(&Workspace::prefix_from_slug(&slug)).await?;
        let workspace = Workspace {
            id: WorkspaceId::generate(),
            name,
            issue_prefix,
            slug,
            description: String::new(),
            context: String::new(),
            logo_hash: None,
            issue_counter: 0,
            default_workflow_id: None,
            git_settings: GitSettings::default(),
            created_by: creator,
            created_at: chrono::Utc::now(),
        };
        tx.insert_workspace(&workspace).await?;
        if let Some(creator) = creator {
            tx.add_member(workspace.id, creator, Role::Owner).await?;
        }
        tx.append_audit(
            actor,
            AuditAction::WorkspaceChange,
            &workspace.name,
            "created",
        )
        .await?;
        tx.commit().await?;
        Ok(workspace)
    }

    /// The workspaces `thing` lives on. `NotFound` when it doesn't exist.
    pub async fn reach(&self, thing: OnWorkspace<'_>) -> Result<Reach> {
        let mut tx = self.store.read().await?;
        let one = |workspace| Ok(Reach::Any(vec![workspace]));
        match thing {
            OnWorkspace::Workspace(id) => one(tx.get_workspace(id).await?.id),
            OnWorkspace::Task(id) => one(workspace_of_task(&mut tx, id).await?),
            OnWorkspace::Run(id) => {
                let run = tx.get_run(id).await?;
                one(workspace_of_task(&mut tx, run.task_id).await?)
            }
            OnWorkspace::Workflow(id) => one(tx.get_workflow(id).await?.workspace_id),
            OnWorkspace::Repo(id) => one(tx.get_repo(id).await?.workspace_id),
            OnWorkspace::Project(id) => one(tx.get_project(id).await?.workspace_id),
            OnWorkspace::Outbox(id) => {
                let row = tx
                    .outbox_with_status(OutboxStatus::Unknown, 500)
                    .await?
                    .into_iter()
                    .find(|row| row.id == id)
                    .ok_or_else(|| Error::NotFound(format!("outbox item {id}")))?;
                match row.run_id {
                    Some(run) => {
                        let task = tx.get_run(run).await?.task_id;
                        one(workspace_of_task(&mut tx, task).await?)
                    }
                    None => Ok(Reach::All(Vec::new())),
                }
            }
            OnWorkspace::Item(item) => {
                describe(&mut tx, item).await?;
                Ok(Reach::Any(tx.workspaces_of(item).await?))
            }
            OnWorkspace::Doc(id) => {
                let doc = tx.get_doc(id).await?;
                let item = WorkspaceItem::KnowledgeSource(&doc.source_id);
                Ok(Reach::Any(tx.workspaces_of(item).await?))
            }
            OnWorkspace::Artifact(id) => {
                let artifact = tx.get_artifact(id).await?;
                one(workspace_of_task(&mut tx, artifact.task_id).await?)
            }
            OnWorkspace::Eval(id) => {
                let eval = tx.get_eval_run(id).await?;
                match eval.spec.get("pick").cloned().map(serde_json::from_value) {
                    Some(Ok(pick)) => reach_of_pick(&mut tx, &pick).await,
                    // An unreadable spec: only who sees every workspace.
                    _ => Ok(Reach::All(all_workspaces(&mut tx).await?)),
                }
            }
            OnWorkspace::EvalSet(id) => {
                let set = tx.get_eval_set(id).await?;
                Ok(Reach::All(
                    workspaces_of_tasks(&mut tx, &set.task_ids).await?,
                ))
            }
            OnWorkspace::Pick(pick) => reach_of_pick(&mut tx, pick).await,
            OnWorkspace::VcsConnection(id) => one(tx.get_connection(id).await?.workspace_id),
            OnWorkspace::Agent(id) => one(tx.get_agent(id).await?.workspace_id),
            OnWorkspace::MemoryItem(id) => one(tx.get_memory(id).await?.workspace_id),
            OnWorkspace::Correction(id) => {
                let task = tx.get_correction(id).await?.task_id;
                one(workspace_of_task(&mut tx, task).await?)
            }
            OnWorkspace::Note(id) => {
                let run = tx.get_note(id).await?.run_id;
                one(tx.workspace_of_run(run).await?)
            }
        }
    }

    /// Changes the profile fields `change` names. The default workflow must
    /// be one of this workspace's. The audit row names the fields, not their
    /// values.
    pub async fn update_workspace_profile(
        &self,
        id: WorkspaceId,
        change: &ProfileChange,
        actor: &str,
    ) -> Result<Workspace> {
        let mut fields = Vec::new();
        let mut tx = self.store.begin().await?;
        let mut workspace = tx.get_workspace(id).await?;
        let before = workspace.name.clone();
        if let Some(name) = &change.name {
            workspace.name = Workspace::clean_name(name)?;
        }
        if let Some(description) = &change.description {
            workspace.description = Workspace::clean_description(description)?;
            fields.push("description");
        }
        if let Some(context) = &change.context {
            workspace.context = Workspace::clean_context(context)?;
            fields.push("context");
        }
        if let Some(workflow) = change.default_workflow_id {
            // A workflow elsewhere, or a hidden one, answers like a missing one.
            if let Some(workflow) = workflow {
                let on_it = match tx.get_workflow(workflow).await {
                    Ok(w) => {
                        w.workspace_id == id
                            && tx.workflow_owner(workflow).await? == WorkflowOwner::Person
                    }
                    Err(sun_store::StoreError::Core(Error::NotFound(_))) => false,
                    Err(e) => return Err(e.into()),
                };
                if !on_it {
                    return Err(Error::InvalidInput(
                        "Pick a default workflow on this workspace.".into(),
                    ));
                }
            }
            workspace.default_workflow_id = workflow;
            fields.push("default workflow");
        }
        if let Some(patch) = change.git_settings
            && patch.apply(workspace.git_settings) != workspace.git_settings
        {
            workspace.git_settings = patch.apply(workspace.git_settings);
            fields.push("git settings");
        }
        let mut details = Vec::new();
        if workspace.name != before {
            details.push(format!("renamed from {before}"));
        }
        if !fields.is_empty() {
            details.push(format!("changed {}", fields.join(", ")));
        }
        if details.is_empty() {
            return Ok(workspace);
        }
        tx.update_workspace(&workspace).await?;
        let detail = details.join("; ");
        tx.append_audit(
            actor,
            AuditAction::WorkspaceChange,
            &workspace.name,
            &detail,
        )
        .await?;
        tx.commit().await?;
        Ok(workspace)
    }

    /// Sets the issue prefix (trimmed, uppercased, 1–10 of `A-Z 0-9`, no
    /// other workspace's). Keys are computed from it, so every task's key
    /// changes with it; none is stored.
    pub async fn set_issue_prefix(
        &self,
        id: WorkspaceId,
        prefix: &str,
        actor: &str,
    ) -> Result<Workspace> {
        let prefix = Workspace::clean_prefix(prefix)?;
        let mut tx = self.store.begin().await?;
        let mut workspace = tx.get_workspace(id).await?;
        let detail = format!("issue prefix {} -> {prefix}", workspace.issue_prefix);
        workspace.issue_prefix = prefix;
        tx.update_workspace(&workspace).await?;
        tx.append_audit(
            actor,
            AuditAction::WorkspaceChange,
            &workspace.name,
            &detail,
        )
        .await?;
        tx.commit().await?;
        Ok(workspace)
    }

    /// Sets the logo: `(bytes, content_type)` of a PNG, JPEG or WebP image of
    /// at most 1 MB whose magic bytes match the type. `None` removes it. The
    /// image is the workspace's own row: replacing or removing it leaves
    /// nothing behind, and it goes with the workspace.
    pub async fn set_logo(
        &self,
        id: WorkspaceId,
        logo: Option<(Vec<u8>, &str)>,
        actor: &str,
    ) -> Result<Workspace> {
        let logo = logo
            .map(|(bytes, content_type)| {
                Workspace::check_logo(&bytes, content_type).map(|kind| (kind, bytes))
            })
            .transpose()?;
        let mut tx = self.store.begin().await?;
        let name = tx.get_workspace(id).await?.name;
        tx.set_logo(
            id,
            logo.as_ref().map(|(kind, bytes)| (*kind, bytes.as_slice())),
        )
        .await?;
        let detail = match logo {
            Some(_) => "logo set",
            None => "logo removed",
        };
        tx.append_audit(actor, AuditAction::WorkspaceChange, &name, detail)
            .await?;
        let workspace = tx.get_workspace(id).await?;
        tx.commit().await?;
        Ok(workspace)
    }

    /// The logo's bytes and media type; `NotFound` when there is none.
    pub async fn logo(&self, id: WorkspaceId) -> Result<(Vec<u8>, String)> {
        let mut tx = self.store.read().await?;
        tx.get_workspace(id).await?;
        let (content_type, bytes) = tx
            .logo(id)
            .await?
            .ok_or_else(|| Error::NotFound(format!("logo of workspace {id}")))?;
        Ok((bytes, content_type))
    }

    /// Deletes an empty workspace (no workflows, no tasks). Main can't be
    /// deleted: it is where workflows without a workspace go.
    pub async fn delete_workspace(&self, id: WorkspaceId, actor: &str) -> Result<()> {
        if id == WorkspaceId::MAIN {
            return Err(Error::InvalidInput(
                "Main is the default workspace and can't be deleted.".into(),
            ));
        }
        let mut tx = self.store.begin().await?;
        let workspace = tx.get_workspace(id).await?;
        tx.delete_workspace(id).await?;
        tx.append_audit(
            actor,
            AuditAction::WorkspaceChange,
            &workspace.name,
            "deleted",
        )
        .await?;
        Ok(tx.commit().await?)
    }

    /// The tasks on a workspace, highest priority first, then oldest first.
    pub async fn tasks_of_workspace(&self, id: WorkspaceId) -> Result<Vec<Task>> {
        let mut tx = self.store.read().await?;
        tx.get_workspace(id).await?;
        Ok(tx.tasks_of_workspace(id).await?)
    }

    /// The workspace a workflow is on.
    pub async fn workspace_of_workflow(&self, id: WorkflowId) -> Result<WorkspaceId> {
        Ok(self
            .store
            .read()
            .await?
            .get_workflow(id)
            .await?
            .workspace_id)
    }

    /// The workspace a task is on.
    pub async fn workspace_of_task(&self, id: TaskId) -> Result<WorkspaceId> {
        Ok(self.store.read().await?.get_task(id).await?.workspace_id)
    }

    /// The workflows of a workspace with their latest version numbers (0 if none saved).
    pub async fn workflows_of_workspace(&self, id: WorkspaceId) -> Result<Vec<(Workflow, u32)>> {
        let mut tx = self.store.read().await?;
        tx.get_workspace(id).await?;
        let mut out = Vec::new();
        for workflow in tx.workflows_of_workspace(id).await? {
            let version = tx
                .latest_workflow_version(workflow.id)
                .await?
                .map_or(0, |v| v.version);
            out.push((workflow, version));
        }
        Ok(out)
    }

    /// Links an existing knowledge source, host or catalog install to
    /// `workspace` too (a knowledge source: the same row, so one index).
    /// A no-op when it is already there (a host attached through a share
    /// becomes attached directly). Audited. `PlanLimit` for a host `plan`
    /// has no room for.
    pub async fn import_to_workspace(
        &self,
        workspace: WorkspaceId,
        item: WorkspaceItem<'_>,
        actor: &str,
        plan: &Entitlement,
    ) -> Result<()> {
        self.import(workspace, item, None, actor, plan).await
    }

    /// [`Self::import_to_workspace`] for a host `member` may attach only
    /// because it is shared with a group they are in
    /// ([`crate::AttachPath::Share`]): the workspace keeps it while that
    /// holds.
    pub async fn attach_shared_host(
        &self,
        workspace: WorkspaceId,
        host: HostId,
        member: MemberId,
        actor: &str,
        plan: &Entitlement,
    ) -> Result<()> {
        let item = WorkspaceItem::Host(host);
        self.import(workspace, item, Some(member), actor, plan)
            .await
    }

    async fn import(
        &self,
        workspace: WorkspaceId,
        item: WorkspaceItem<'_>,
        shared_via: Option<MemberId>,
        actor: &str,
        plan: &Entitlement,
    ) -> Result<()> {
        let mut tx = self.store.begin().await?;
        let workspace_name = tx.get_workspace(workspace).await?.name;
        let what = describe(&mut tx, item).await?;
        let already = tx.workspaces_of(item).await?.contains(&workspace);
        match item {
            WorkspaceItem::Host(host) => {
                if !already {
                    plan.check(Quota::Hosts, tx.hosts_in(workspace).await?.len())?;
                }
                tx.attach_host(workspace, host, shared_via).await?;
            }
            _ if already => {}
            _ => tx.link(workspace, item).await?,
        }
        if !already {
            let via = match shared_via {
                Some(_) => " (shared with a group of theirs)",
                None => "",
            };
            tx.append_audit(
                actor,
                AuditAction::WorkspaceChange,
                &workspace_name,
                &format!("imported {what}{via}"),
            )
            .await?;
        }
        tx.commit().await?;
        if !already {
            wake_queue_for(self, item);
        }
        Ok(())
    }

    /// Removes an item from `workspace` (`NotFound` when it isn't there);
    /// true when no workspace has it any more. A knowledge source on no
    /// workspace left is deleted with its docs.
    pub async fn detach_from_workspace(
        &self,
        workspace: WorkspaceId,
        item: WorkspaceItem<'_>,
        actor: &str,
    ) -> Result<bool> {
        let mut tx = self.store.begin().await?;
        let workspace_name = tx.get_workspace(workspace).await?.name;
        let what = describe(&mut tx, item).await?;
        if !tx.unlink(workspace, item).await? {
            return Err(Error::NotFound(format!(
                "{what} on workspace {workspace_name}"
            )));
        }
        let orphan = tx.workspaces_of(item).await?.is_empty();
        let detail = match item {
            WorkspaceItem::KnowledgeSource(id) if orphan => {
                tx.delete_knowledge_source(id).await?;
                format!("deleted {what}")
            }
            _ => format!("removed {what}"),
        };
        tx.append_audit(
            actor,
            AuditAction::WorkspaceChange,
            &workspace_name,
            &detail,
        )
        .await?;
        tx.commit().await?;
        wake_queue_for(self, item);
        Ok(orphan)
    }
}

/// A host joining or leaving a workspace changes where its waiting runs may
/// go: re-place them now (a run only it could take is refused), not on the
/// next unrelated queue event.
fn wake_queue_for(engine: &Engine, item: WorkspaceItem<'_>) {
    if let WorkspaceItem::Host(_) = item {
        engine.queue_wake.notify_one();
    }
}

/// `knowledge source <name>`, `host <name>` or `catalog entry <id>`;
/// `NotFound` when a source or host is missing.
async fn describe(tx: &mut Tx, item: WorkspaceItem<'_>) -> Result<String> {
    Ok(match item {
        WorkspaceItem::KnowledgeSource(id) => {
            format!(
                "knowledge source {}",
                tx.get_knowledge_source(id).await?.name
            )
        }
        WorkspaceItem::Host(id) => format!("host {}", tx.get_host(id).await?.name),
        WorkspaceItem::CatalogInstall(id) => format!("catalog entry {id}"),
    })
}
