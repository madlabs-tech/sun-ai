//! Repo-scope skill installs: a catalog skill is committed into
//! the repo's native skill folder(s) on branch `sun/skill-<name>`, pushed from
//! the engine's host and opened as a PR by the git host mod's `open_pr`
//! through the **transactional outbox**: the same push + `open_pr` path as a
//! memory PR (`memory`), no second implementation.
//!
//! An outbox row belongs to a run, so each (repo, skill) gets one anchor: a
//! `done` task bound to the repo with a `succeeded` run of the built-in
//! `skill-install-<mod>` workflow. The PR shows on that task too. Idempotent
//! per (repo, skill, version): a repeat returns the PR already enqueued; a
//! new catalog version commits on the same branch and asks for the PR again.
//! The committed copies carry no `.gitignore` marker, so session sync
//! (the host's skills sync) never touches them.

use serde::{Deserialize, Serialize};
use serde_json::json;
use sun_core::host_protocol::{ExecRequest, Worktree};
use sun_core::model::{OutboxItem, OutboxStatus, Repo, Run, Task};
use sun_core::{
    Error, IdemKey, OutboxId, RepoId, Result, RunId, RunStatus, StepKey, TaskId, TaskStatus,
};
use sun_store::{EventLog, OutboxRepo, RepoRepo, RunRepo, SettingRepo, TaskRepo};

use crate::Engine;
use crate::executor::kind;
use crate::gates::quote;
use crate::memory::{PrView, git_mod};
use crate::outbox::repo_slug;
use crate::push;

/// A repo-scope skill install: its branch and where its PR stands.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SkillPr {
    /// The repo.
    pub repo_id: RepoId,
    /// The skill (catalog id).
    pub skill: String,
    /// The catalog version the PR carries.
    pub version: String,
    /// `sun/skill-<name>`.
    pub branch: String,
    /// The anchor task the PR also shows on.
    pub task_id: TaskId,
    /// The `open_pr` outbox row's state; `None` until pushed and enqueued.
    pub pr: Option<PrView>,
}

/// What `kernel_setting` keeps per (repo, skill).
#[derive(Serialize, Deserialize)]
struct Record {
    task_id: TaskId,
    run_id: RunId,
    version: String,
    /// The `open_pr` row's idem key, once enqueued for `version`.
    pr_key: Option<IdemKey>,
}

fn setting_key(repo: RepoId, skill: &str) -> String {
    format!("skill_pr:{repo}:{skill}")
}

fn branch(skill: &str) -> String {
    format!("sun/skill-{skill}")
}

/// Shell that checks out `branch` and writes the skill into each of `dirs`:
/// file `i` comes from env `SUN_SKILL_<i>` (no file content in argv), the old
/// copy is replaced (files dropped upstream go), and the folders are staged
/// even when the repo's `.gitignore` covers them.
fn write_script(branch: &str, skill: &str, dirs: &[&str], paths: &[&str]) -> String {
    let mut script = format!("git checkout -q -B {} || exit 1\n", quote(branch));
    for dir in dirs {
        let root = format!("{dir}/{skill}");
        script.push_str(&format!(
            "rm -rf {0} && mkdir -p {0} || exit 1\n",
            quote(&root)
        ));
        for (i, path) in paths.iter().enumerate() {
            let at = format!("{root}/{path}");
            if let Some((parent, _)) = at.rsplit_once('/') {
                script.push_str(&format!("mkdir -p {} || exit 1\n", quote(parent)));
            }
            script.push_str(&format!(
                "printf '%s' \"$SUN_SKILL_{i}\" > {} || exit 1\n",
                quote(&at)
            ));
        }
        script.push_str(&format!("git add -f -- {} || exit 1\n", quote(&root)));
    }
    script
}

/// A skill name or file path that is safe to join under a skills folder.
fn check_path(path: &str) -> Result<()> {
    let ok = !path.is_empty()
        && !path.starts_with('/')
        && path
            .split('/')
            .all(|part| !part.is_empty() && part != "." && part != "..");
    if ok {
        Ok(())
    } else {
        Err(Error::InvalidInput(format!("skill path `{path}`")))
    }
}

impl Engine {
    /// Opens (or returns) the PR that commits skill `skill` at `version`
    /// (`files` as (path inside the skill, bytes)) into repo `repo_id`, in the
    /// folders the `runtimes` read (`None` = every runtime). Pushes from the
    /// engine's host, then enqueues the git host mod's `open_pr`.
    pub async fn install_skill_pr(
        &self,
        repo_id: RepoId,
        skill: &str,
        version: &str,
        files: &[(String, Vec<u8>)],
        runtimes: Option<&[String]>,
    ) -> Result<SkillPr> {
        check_path(skill)?;
        if skill.contains('/') {
            return Err(Error::InvalidInput(format!("skill name `{skill}`")));
        }
        let dirs = sun_core::repo_skill_dirs(runtimes);
        if dirs.is_empty() {
            return Err(Error::InvalidInput(
                "none of these runtimes reads skills from a repo folder".into(),
            ));
        }
        let mut env = Vec::new();
        for (i, (path, data)) in files.iter().enumerate() {
            check_path(path)?;
            let text = String::from_utf8(data.clone())
                .ok()
                .filter(|t| !t.contains('\0'))
                .ok_or_else(|| Error::InvalidInput(format!("`{path}` is not text")))?;
            env.push((format!("SUN_SKILL_{i}"), text));
        }

        let (repo, record) = {
            let mut tx = self.store.read().await?;
            let repo = tx.get_repo(repo_id).await?;
            let record: Option<Record> = tx
                .setting(&setting_key(repo_id, skill))
                .await?
                .map(serde_json::from_value)
                .transpose()
                .map_err(|e| Error::InvalidInput(format!("skill PR record: {e}")))?;
            (repo, record)
        };
        let record = match record {
            Some(r) if r.version == version && r.pr_key.is_some() => {
                return self.view(repo_id, skill, r).await;
            }
            Some(r) => r,
            None => self.skill_anchor(&repo, skill, version).await?,
        };

        let mod_name = git_mod(&repo.url);
        let branch = branch(skill);
        let worktree = Worktree::Repo {
            task_id: record.task_id,
            url: repo.url.clone(),
            base: Some(repo.default_branch.clone()),
            branch: branch.clone(),
        };
        let cwd = self
            .hosts
            .prepare_worktree(self.host, worktree.clone())
            .await?;
        let paths: Vec<&str> = files.iter().map(|(p, _)| p.as_str()).collect();
        let request = ExecRequest {
            cwd,
            command: write_script(&branch, skill, &dirs, &paths),
            env: env.into_iter().collect(),
            timeout_s: 60,
        };
        let out = self.hosts.exec(self.host, request).await?;
        if out.code != Some(0) {
            return Err(Error::Unavailable(format!(
                "write the {skill} skill: {}",
                out.output.trim()
            )));
        }
        let message = format!("feat(skills): add the {skill} skill ({version})");
        let job = {
            let mut tx = self.store.read().await?;
            push::Job::branch(
                &mut tx,
                self.host,
                worktree,
                mod_name,
                branch.clone(),
                message,
            )
            .await?
        };
        job.run(&*self.hosts).await.map_err(Error::Unavailable)?;

        let key = IdemKey::new(
            record.run_id,
            &StepKey::new("skill")?,
            &format!("pr:{version}"),
        );
        let folders = dirs
            .iter()
            .map(|d| format!("`{d}/{skill}`"))
            .collect::<Vec<_>>()
            .join(" and ");
        let row = OutboxItem {
            id: OutboxId::generate(),
            run_id: Some(record.run_id),
            idem_key: key.clone(),
            target: format!("{mod_name}.open_pr"),
            payload: json!({
                "repo": repo_slug(&repo.url),
                "head": branch,
                "base": repo.default_branch,
                "title": format!("Add the {skill} skill"),
                "body": format!(
                    "Sun adds the `{skill}` skill (catalog version {version}) to {folders}, \
                     so coding agents working in this repo load it.\n\n\
                     Merge to keep it, or close the PR to leave it out. \
                     Opened by Sun for task {}.",
                    record.task_id
                ),
            }),
            status: OutboxStatus::Pending,
            result: None,
        };
        let record = Record {
            version: version.to_owned(),
            pr_key: Some(key),
            ..record
        };
        let mut tx = self.store.begin().await?;
        tx.enqueue(&row).await?;
        tx.put_setting(&setting_key(repo_id, skill), &json!(record))
            .await?;
        tx.commit().await?;
        self.outbox_wake.notify_one();
        self.view(repo_id, skill, record).await
    }

    /// The repo-scope install of `skill` into `repo_id`, if one was asked for.
    pub async fn skill_pr(&self, repo_id: RepoId, skill: &str) -> Result<Option<SkillPr>> {
        let record = self
            .store
            .read()
            .await?
            .setting(&setting_key(repo_id, skill))
            .await?;
        match record {
            Some(value) => {
                let record = serde_json::from_value(value)
                    .map_err(|e| Error::InvalidInput(format!("skill PR record: {e}")))?;
                Ok(Some(self.view(repo_id, skill, record).await?))
            }
            None => Ok(None),
        }
    }

    /// Creates the anchor task and finished run (see the module docs).
    async fn skill_anchor(&self, repo: &Repo, skill: &str, version: &str) -> Result<Record> {
        let name = format!("skill-install-{}", git_mod(&repo.url));
        let found = self
            .workflows()
            .await?
            .into_iter()
            .find(|(w, v)| w.name == name && *v > 0);
        let (workflow_id, workflow_version) = match found {
            Some((w, v)) => (w.id, v),
            None => {
                let yaml = format!(
                    "name: {name}\n\
                     steps:\n  - id: open_pr\n    type: action\n    use: {}.open_pr\n",
                    git_mod(&repo.url)
                );
                let (w, v) = self.save_workflow(None, &yaml).await?;
                (w.id, v.version)
            }
        };
        let workspace = self.workspace_of_workflow(workflow_id).await?;
        let task = Task {
            body: format!(
                "Catalog install into the repo, by PR on `{}`.",
                branch(skill)
            ),
            workflow_id: Some(workflow_id),
            status: TaskStatus::Done,
            ..Task::new(
                workspace,
                format!("Add the {skill} skill to {}", repo_slug(&repo.url)),
            )
        };
        let run = Run {
            id: RunId::generate(),
            task_id: task.id,
            workflow_id,
            workflow_version,
            status: RunStatus::Succeeded,
            end_reason: None,
            budget: Default::default(),
            repo_ids: vec![repo.id],
        };
        let record = Record {
            task_id: task.id,
            run_id: run.id,
            version: version.to_owned(),
            pr_key: None,
        };
        let mut tx = self.store.begin().await?;
        tx.insert_task(&task).await?;
        tx.insert_run(&run).await?;
        tx.put_setting(&setting_key(repo.id, skill), &json!(record))
            .await?;
        tx.commit().await?;
        Ok(record)
    }

    /// `record` with its PR's state, read from the anchor run's outbox events.
    async fn view(&self, repo_id: RepoId, skill: &str, record: Record) -> Result<SkillPr> {
        let pr = match &record.pr_key {
            Some(key) => {
                let mut pr = PrView {
                    status: OutboxStatus::Pending,
                    url: None,
                };
                let mut tx = self.store.read().await?;
                let mut after = 0;
                loop {
                    let page = tx.events_after(Some(record.run_id), after, 500).await?;
                    let Some(last) = page.last() else { break };
                    after = last.seq;
                    for event in page.iter().filter(|e| e.payload["idem_key"] == json!(key)) {
                        pr.status = match event.kind.as_str() {
                            kind::OUTBOX_DELIVERED => OutboxStatus::Delivered,
                            kind::OUTBOX_FAILED => OutboxStatus::Failed,
                            _ => OutboxStatus::Unknown,
                        };
                        pr.url = event.payload["detail"]["url"].as_str().map(str::to_owned);
                    }
                }
                Some(pr)
            }
            None => None,
        };
        Ok(SkillPr {
            repo_id,
            skill: skill.to_owned(),
            version: record.version,
            branch: branch(skill),
            task_id: record.task_id,
            pr,
        })
    }
}

#[cfg(test)]
mod tests;
