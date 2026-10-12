//! **Transactional outbox** worker.
//!
//! The executor enqueues side effects in the same transaction as the step
//! change. This worker delivers them after commit through [`ModLink`] and
//! reports each [`Settlement`] back; the executor records it together with the
//! step move, in one transaction.
//!
//! Exactly-once rules:
//! - A row goes `pending → unknown` in its own committed transaction **before**
//!   the mod is called. From then on its outcome is unknown until recorded, so
//!   a crash mid-call can never lead to a blind resend.
//! - A timeout (or `Error::Unavailable`) leaves the row `unknown`.
//! - `unknown` rows are only resolved by asking the mod (`reconcile`, on
//!   start-up) or by a human. Only a mod's explicit "not delivered" puts a row
//!   back to `pending`.
//! - Every call carries the row's [`IdemKey`] so the mod can dedupe too.
//! - A row being delivered is `unknown` too, so it is also in [`InFlight`]
//!   until the executor records its settlement: a human decision waits for
//!   that (a double-clicked `resend` never sends twice).
//!
//! Blobs: a row stores blob hashes (`files: [{name, blob, content_type}]`);
//! the worker swaps each `blob` for `content_base64` when it delivers, since a
//! mod can't read the kernel's blob store. See [`inline_blobs`].

use std::collections::HashSet;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use serde_json::{Value, json};
use sun_core::model::{Artifact, OutboxItem, OutboxStatus, Repo, Task};
use sun_core::{BlobHash, Error, IdemKey, ModCall, ModLink, OutboxId, Result, RunId};
use sun_store::{
    ArtifactRepo, BlobStore, OutboxRepo, RepoRepo, RunRepo, SettingRepo, Store, TaskRepo, Tx,
};
use sun_workflow::{Step as StepDef, StepKind};
use tokio::sync::{Notify, mpsc};

/// Action suffix of a delivery plugin's upload (`gdrive.upload`):
/// its row carries the task's artifacts as `files`.
pub(crate) const UPLOAD: &str = ".upload";

/// Rows read per delivery pass.
const BATCH: u32 = 32;
/// Most blob bytes inlined into one delivery (device screenshots are capped
/// at 8 MiB per verify); about 11 MiB once base64-encoded.
const MAX_INLINE: usize = 8 * 1024 * 1024;

/// How a delivery ended.
#[derive(Debug)]
pub(crate) enum Outcome {
    /// The mod applied it; its reply.
    Delivered(Value),
    /// The mod refused it definitively.
    Failed(String),
    /// No answer (timeout, mod unreachable): waits for reconcile or a human.
    Unknown(String),
}

/// Rows the worker is delivering now; the worker adds one before marking it
/// `unknown`, the executor removes it when it records the settlement.
pub(crate) type InFlight = Arc<Mutex<HashSet<OutboxId>>>;

/// Locks `in_flight` (a poisoned set is still a valid set).
pub(crate) fn in_flight(set: &InFlight) -> std::sync::MutexGuard<'_, HashSet<OutboxId>> {
    set.lock().unwrap_or_else(PoisonError::into_inner)
}

/// A delivery result for the executor to record.
#[derive(Debug)]
pub(crate) struct Settlement {
    /// The row.
    pub item: OutboxItem,
    /// What happened.
    pub outcome: Outcome,
}

/// The outbox rows a step enqueues when it starts (empty for steps without
/// side effects). Keys are stable per run and step, so a re-enqueue is a no-op.
///
/// - `action`: one row, target = `use:` (e.g. `github.open_pr`), params =
///   `with:` with its placeholders filled ([`fill`]). A `<git host>.open_pr`
///   is filled from the run (see [`RunFacts::open_pr`]); `with:` keys win.
///   A `with:` that reads `${trigger.*}` on a task no trigger made has no
///   source to report to: no row (the step succeeds).
///   A `<plugin>.upload` gets `{title, files}` with the latest version of
///   each task artifact (`with: {files: [names]}` picks some); its other
///   `with:` keys win.
/// - `notify`: one row per `to:` entry `channel:address`, target
///   `<channel>.notify`, params `{key, channel, text}` (mod-slack's contract).
///   `key` is the row's idem key, so each row owns one message.
pub(crate) fn side_effects(run: &RunFacts, def: &StepDef) -> Vec<OutboxItem> {
    let item = |idem_key: IdemKey, target: String, payload: Value| OutboxItem {
        id: OutboxId::generate(),
        run_id: Some(run.run_id),
        idem_key,
        target,
        payload,
        status: OutboxStatus::Pending,
        result: None,
    };
    match &def.kind {
        StepKind::Action { target, with } => {
            // A multi-repo task opens one PR per repo, each keyed by its repo.
            let rows: Vec<(String, Value)> = match target.ends_with(".open_pr") {
                true if run.multi => run
                    .repos
                    .iter()
                    .map(|repo| {
                        (
                            format!("{target}:{}", repo_slug(&repo.url)),
                            run.open_pr(repo),
                        )
                    })
                    .collect(),
                true => vec![(
                    target.clone(),
                    run.repos.first().map_or(Value::Null, |r| run.open_pr(r)),
                )],
                false if target.ends_with(UPLOAD) => vec![(target.clone(), run.upload(with))],
                false => vec![(target.clone(), Value::Null)],
            };
            // Reporting back to a source the task doesn't have (a manual
            // run): nothing to send, so the step succeeds.
            let reads_trigger = with
                .as_ref()
                .is_some_and(|w| w.to_string().contains("${trigger."));
            if reads_trigger && run.trigger.is_null() {
                return Vec::new();
            }
            let with = with.as_ref().map(|w| fill(w, run));
            rows.into_iter()
                .map(|(action_key, mut params)| {
                    // An upload's `with:` is already in its params (`upload`).
                    let with = with.as_ref().filter(|_| !target.ends_with(UPLOAD));
                    match (&mut params, with) {
                        (Value::Object(defaults), Some(Value::Object(with))) => {
                            defaults.extend(with.clone());
                        }
                        (_, Some(with)) => params = with.clone(),
                        (_, None) => {}
                    }
                    let key = IdemKey::new(run.run_id, &def.id, &action_key);
                    item(key, target.clone(), params)
                })
                .collect()
        }
        StepKind::Notify { to, message } => to
            .iter()
            .map(|to| {
                let key = IdemKey::new(run.run_id, &def.id, &format!("notify:{to}"));
                let text = message.clone().unwrap_or_else(|| run.task.title.clone());
                notify_item(Some(run.run_id), key, to, &text)
            })
            .collect(),
        _ => Vec::new(),
    }
}

/// `with:` with every `${…}` in its strings filled from the run:
/// `${trigger.<field>[.<field>…]}` from the payload of the event that
/// created the task (a string as is, other values as JSON; missing: empty),
/// `${pr.url}` the PRs this run opened (space-separated; none: empty),
/// `${task.id}` and `${task.title}`. Anything else stays as written.
pub(crate) fn fill(with: &Value, run: &RunFacts) -> Value {
    let lookup = |name: &str| -> Option<String> {
        let text = |v: &Value| match v {
            Value::Null => String::new(),
            Value::String(s) => s.clone(),
            v => v.to_string(),
        };
        Some(match name {
            "pr.url" => run.prs.join(" "),
            "task.id" => run.task.id.to_string(),
            "task.title" => run.task.title.clone(),
            _ => {
                let path = name.strip_prefix("trigger.")?;
                text(path.split('.').fold(&run.trigger, |v, k| &v[k]))
            }
        })
    };
    match with {
        Value::String(s) => {
            let mut out = String::new();
            let mut rest = s.as_str();
            while let Some(start) = rest.find("${") {
                let Some(len) = rest[start..].find('}') else {
                    break;
                };
                let name = &rest[start + 2..start + len];
                out.push_str(&rest[..start]);
                match lookup(name) {
                    Some(value) => out.push_str(&value),
                    None => out.push_str(&rest[start..=start + len]),
                }
                rest = &rest[start + len + 1..];
            }
            out.push_str(rest);
            Value::String(out)
        }
        Value::Array(items) => Value::Array(items.iter().map(|v| fill(v, run)).collect()),
        Value::Object(fields) => Value::Object(
            fields
                .iter()
                .map(|(k, v)| (k.clone(), fill(v, run)))
                .collect(),
        ),
        v => v.clone(),
    }
}

/// One notifier message as an outbox row: `to` = `channel:address` (e.g.
/// `slack:#eng`), target `<channel>.notify`, params `{key, channel, text}`
/// (mod-slack's contract; `key` makes the row own one message).
pub(crate) fn notify_item(run_id: Option<RunId>, key: IdemKey, to: &str, text: &str) -> OutboxItem {
    let (channel, address) = to.split_once(':').unwrap_or((to, ""));
    OutboxItem {
        id: OutboxId::generate(),
        run_id,
        payload: json!({ "key": key.as_str(), "channel": address, "text": text }),
        idem_key: key,
        target: format!("{channel}.notify"),
        status: OutboxStatus::Pending,
        result: None,
    }
}

/// What a run's side effects are filled from: its task and the run's
/// repos (`run.repo_ids`), in merge order.
pub(crate) struct RunFacts {
    pub run_id: RunId,
    pub task: Task,
    /// The task's branch on the host worktree and the remote
    /// (`tasks::branch_of`).
    pub branch: String,
    pub repos: Vec<Repo>,
    /// The run works in several repos: one PR per (changed) repo.
    pub multi: bool,
    /// Who ran each agent step and who paid, in order.
    pub runtimes: Vec<crate::executor::failover::RanOn>,
    /// Payload of the event that created the task (`Null`: none).
    pub trigger: Value,
    /// URLs of the PRs this run's `open_pr` rows opened so far.
    pub prs: Vec<String>,
    /// The latest version of each task artifact, by name.
    pub artifacts: Vec<Artifact>,
}

impl RunFacts {
    // ponytail: scans every delivered row for the run's PRs; index outbox
    // by run when the table grows large.
    pub(crate) async fn load(tx: &mut Tx, run_id: RunId) -> Result<Self> {
        let run = tx.get_run(run_id).await?;
        let task_id = run.task_id;
        let task = tx.get_task(task_id).await?;
        let branch = crate::executor::tasks::branch_of(tx, &task).await?;
        let mut repos = Vec::new();
        for id in &run.repo_ids {
            repos.push(tx.get_repo(*id).await?);
        }
        let trigger = tx
            .setting(&format!("{}{task_id}", crate::triggers::TRIGGER_OF))
            .await?
            .unwrap_or(Value::Null);
        let prs = tx
            .outbox_with_status(OutboxStatus::Delivered, u32::MAX)
            .await?
            .into_iter()
            .filter(|row| row.run_id == Some(run_id) && row.target.ends_with(".open_pr"))
            .filter_map(|row| row.result?["url"].as_str().map(str::to_owned))
            .collect();
        Ok(Self {
            run_id,
            task,
            branch,
            multi: repos.len() > 1,
            repos,
            runtimes: crate::executor::failover::runtimes_of_run(tx, run_id).await?,
            trigger,
            prs,
            artifacts: latest(tx.artifacts_of(task_id).await?),
        })
    }

    /// An `.upload` row: `{title, files: [{name, blob, content_type}]}`,
    /// then `with:` (whose `files`, a list of names, picks the artifacts).
    fn upload(&self, with: &Option<Value>) -> Value {
        let mut params = with.as_ref().map_or_else(|| json!({}), |w| fill(w, self));
        let picked: Option<Vec<&str>> = params["files"]
            .as_array()
            .map(|names| names.iter().filter_map(Value::as_str).collect());
        let files: Vec<Value> = self
            .artifacts
            .iter()
            .filter(|a| picked.as_ref().is_none_or(|p| p.contains(&a.name.as_str())))
            .map(|a| json!({ "name": a.name, "blob": a.blob, "content_type": a.mime }))
            .collect();
        if let Value::Object(map) = &mut params {
            map.entry("title")
                .or_insert_with(|| self.task.title.clone().into());
            map.insert("files".into(), files.into());
        }
        params
    }

    /// The git-host `open_pr` contract (`{repo, head, base, title, body}`)
    /// for the task's branch (the host worktree's) in `repo`. A multi-repo
    /// run's bodies link each other's PR branches in merge order
    /// (`multi_repo::links`).
    fn open_pr(&self, repo: &Repo) -> Value {
        let task = &self.task;
        let mut body = match task.body.trim() {
            "" => String::new(),
            text => format!("{text}\n\n---\n"),
        };
        if self.multi {
            body.push_str(&crate::multi_repo::links(&self.repos, repo, &self.branch));
        }
        body.push_str(&self.runtime_lines());
        body.push_str(&format!("Opened by Sun for task {}.", task.id));
        json!({
            "repo": repo_slug(&repo.url),
            "head": self.branch,
            "base": repo.default_branch,
            "title": task.title,
            "body": body,
        })
    }

    /// `Runtimes:` with one line per agent step, e.g. ``- `implement`:
    /// claude-code (your subscription) → codex (your API key)``; empty when
    /// no agent ran.
    fn runtime_lines(&self) -> String {
        let mut steps: Vec<(&str, Vec<String>)> = Vec::new();
        for ran in &self.runtimes {
            let paid = match ran.source.as_str() {
                "byo_runtime" => " (your subscription)",
                "byo_key" => " (your API key)",
                "credits" => " (Sun credits)",
                _ => "",
            };
            let who = format!("{}{paid}", ran.runtime);
            match steps
                .iter_mut()
                .find(|(key, _)| *key == ran.step_key.as_str())
            {
                Some((_, line)) if line.last() != Some(&who) => line.push(who),
                Some(_) => {}
                None => steps.push((ran.step_key.as_str(), vec![who])),
            }
        }
        if steps.is_empty() {
            return String::new();
        }
        let mut text = "Runtimes:\n".to_owned();
        for (key, line) in steps {
            text.push_str(&format!("- `{key}`: {}\n", line.join(" → ")));
        }
        text + "\n"
    }
}

/// The newest version of each artifact name, in name order.
fn latest(all: Vec<Artifact>) -> Vec<Artifact> {
    let mut by_name = std::collections::BTreeMap::new();
    for artifact in all {
        // Oldest first: a later version replaces an earlier one.
        by_name.insert(artifact.name.clone(), artifact);
    }
    by_name.into_values().collect()
}

/// `owner/name` (or `group/sub/name`) of a clone URL: the path after the
/// host, without `.git`. `https://github.com/acme/web.git`,
/// `git@github.com:acme/web.git` and `ssh://git@host/acme/web` → `acme/web`.
pub(crate) fn repo_slug(url: &str) -> String {
    let path = match url.split_once("://") {
        Some((_, rest)) => rest.split_once('/').map_or("", |(_, path)| path),
        None => url.split_once(':').map_or(url, |(_, path)| path),
    };
    let path = path.trim_matches('/');
    path.strip_suffix(".git").unwrap_or(path).to_owned()
}

/// Runs the worker until the executor goes away or the store fails.
///
/// Reconciles `unknown` rows once, then delivers `pending` rows whenever
/// `wake` fires (the executor notifies after enqueueing; no polling).
pub(crate) async fn run(
    store: Store,
    mods: Arc<dyn ModLink>,
    vcs: crate::vcs::Vcs,
    timeout: Duration,
    wake: Arc<Notify>,
    settle: mpsc::Sender<Settlement>,
    in_flight: InFlight,
) -> Result<()> {
    let worker = Worker {
        store,
        mods,
        vcs,
        timeout,
        settle,
        in_flight,
    };
    let unknown = worker
        .store
        .read()
        .await?
        .outbox_with_status(OutboxStatus::Unknown, u32::MAX)
        .await?;
    for item in unknown {
        worker.reconcile(item).await?;
    }
    loop {
        let batch = worker
            .store
            .read()
            .await?
            .outbox_with_status(OutboxStatus::Pending, BATCH)
            .await?;
        if batch.is_empty() {
            wake.notified().await;
            continue;
        }
        for item in batch {
            worker.deliver(item).await?;
        }
    }
}

struct Worker {
    store: Store,
    mods: Arc<dyn ModLink>,
    /// Git host mods get the task workspace's connection token per call.
    vcs: crate::vcs::Vcs,
    timeout: Duration,
    settle: mpsc::Sender<Settlement>,
    in_flight: InFlight,
}

impl Worker {
    async fn deliver(&self, item: OutboxItem) -> Result<()> {
        in_flight(&self.in_flight).insert(item.id);
        self.mark(item.id, OutboxStatus::Unknown).await?;
        let outcome = match self.call(&item, None).await {
            Ok(reply) => Outcome::Delivered(reply),
            Err(Error::Unavailable(why)) => Outcome::Unknown(why),
            Err(e) => Outcome::Failed(e.to_string()),
        };
        self.report(item, outcome).await
    }

    /// Asks the mod whether an `unknown` row was applied. The reply must be
    /// `{"delivered": true, "result": ...}` or `{"delivered": false}`;
    /// anything else keeps the row `unknown`.
    async fn reconcile(&self, item: OutboxItem) -> Result<()> {
        let outcome = match self.call(&item, Some("reconcile")).await {
            Ok(reply) => match reply.get("delivered") {
                Some(Value::Bool(true)) => {
                    Outcome::Delivered(reply.get("result").cloned().unwrap_or(Value::Null))
                }
                Some(Value::Bool(false)) => {
                    // The mod confirmed it never applied it: safe to send.
                    return self.mark(item.id, OutboxStatus::Pending).await;
                }
                _ => Outcome::Unknown("reconcile: unexpected reply".into()),
            },
            Err(e) => Outcome::Unknown(format!("reconcile: {e}")),
        };
        self.report(item, outcome).await
    }

    /// Calls `target` (`mod.method`) or, with `method`, that method of the
    /// same mod with the original call as params. The kernel's own
    /// `vcs.delete_hook` rows are delivered by `vcs`, not a mod; deleting a
    /// hook again is harmless, so their reconcile just delivers again.
    async fn call(&self, item: &OutboxItem, method: Option<&str>) -> Result<Value> {
        let (mod_name, action) = item
            .target
            .split_once('.')
            .ok_or_else(|| Error::InvalidInput(format!("outbox target `{}`", item.target)))?;
        if item.target == crate::vcs::DELETE_HOOK {
            let deleted = tokio::time::timeout(
                self.timeout,
                crate::vcs::delete_hook(&self.store, &self.vcs, item),
            )
            .await
            .map_err(|_| Error::Unavailable(format!("timed out after {:?}", self.timeout)))??;
            return Ok(match method {
                None => deleted,
                Some(_) => json!({ "delivered": true, "result": deleted }),
            });
        }
        // Reconcile only looks the effect up, so it keeps the hashes.
        let (method, params) = match method {
            None => (
                action,
                inline_blobs(self.store.blobs(), item.payload.clone()).await?,
            ),
            Some(m) => (m, json!({ "method": action, "params": item.payload })),
        };
        let call = ModCall {
            mod_name: mod_name.to_owned(),
            method: method.to_owned(),
            params,
            idem_key: Some(item.idem_key.clone()),
        };
        let task = match item.run_id {
            Some(run_id) => Some(self.store.read().await?.get_run(run_id).await?.task_id),
            None => None,
        };
        // A git host mod gets the workspace's token in the call, never in the row.
        let call = crate::vcs::call_git_host(&self.store, &self.vcs, &*self.mods, task, call);
        tokio::time::timeout(self.timeout, call)
            .await
            .map_err(|_| Error::Unavailable(format!("timed out after {:?}", self.timeout)))?
    }

    async fn mark(&self, id: OutboxId, status: OutboxStatus) -> Result<()> {
        let mut tx = self.store.begin().await?;
        tx.set_outbox_status(id, status, None).await?;
        Ok(tx.commit().await?)
    }

    async fn report(&self, item: OutboxItem, outcome: Outcome) -> Result<()> {
        self.settle
            .send(Settlement { item, outcome })
            .await
            .map_err(|_| Error::Unavailable("engine stopped".into()))
    }
}

/// Replaces `blob` with `content_base64` (the blob's bytes) in each entry of
/// the payload's top-level `files` array, so a mod never gets a hash it can't
/// read. A missing blob or more than [`MAX_INLINE`] bytes fails the delivery.
async fn inline_blobs(blobs: &BlobStore, mut payload: Value) -> Result<Value> {
    let Some(files) = payload.get_mut("files").and_then(Value::as_array_mut) else {
        return Ok(payload);
    };
    let mut total = 0;
    for file in files.iter_mut().filter_map(Value::as_object_mut) {
        let Some(hash) = file.remove("blob") else {
            continue;
        };
        let hash = BlobHash::new(hash.as_str().unwrap_or_default())?;
        let bytes = blobs.get(&hash).await?;
        total += bytes.len();
        if total > MAX_INLINE {
            return Err(Error::InvalidInput(format!(
                "files exceed {MAX_INLINE} bytes"
            )));
        }
        file.insert("content_base64".into(), BASE64.encode(bytes).into());
    }
    Ok(payload)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use serde_json::{Value, json};
    use sun_core::model::{Repo, Task};
    use sun_core::{RepoId, RunId, TaskStatus, WorkflowId};

    use super::{MAX_INLINE, RunFacts, inline_blobs, repo_slug, side_effects};

    fn facts(repo: bool) -> RunFacts {
        RunFacts {
            run_id: RunId::generate(),
            task: Task {
                title: "Retry failed webhooks".into(),
                body: "They are dropped today.".into(),
                workflow_id: Some(WorkflowId::generate()),
                status: TaskStatus::InProgress,
                ..sun_core::model::Task::new(sun_core::WorkspaceId::MAIN, "")
            },
            branch: "sun/MAIN-4-retry-failed-webhooks".into(),
            repos: repo
                .then(|| Repo {
                    id: RepoId::generate(),
                    workspace_id: sun_core::WorkspaceId::MAIN,
                    url: "git@github.com:acme/web.git".into(),
                    default_branch: "trunk".into(),
                    gate_config: json!({}),
                    description: String::new(),
                    hook_id: None,
                })
                .into_iter()
                .collect(),
            multi: false,
            runtimes: Vec::new(),
            trigger: Value::Null,
            prs: Vec::new(),
            artifacts: Vec::new(),
        }
    }

    /// `${…}` in `with:` read the trigger, the run's PRs and the task.
    #[test]
    fn with_placeholders_fill_from_the_trigger_and_the_run() {
        let mut run = facts(false);
        run.trigger = json!({ "issue": "APP-4", "fields": { "n": 7 } });
        run.prs = vec!["https://pr/1".into()];
        let with = json!({ "issue": "${trigger.issue}", "list": ["#${trigger.fields.n}"],
            "body": "PR: ${pr.url} for ${task.title}; ${trigger.nope}|${other} ${open" });
        let task = &run.task.title;
        assert_eq!(
            super::fill(&with, &run),
            json!({ "issue": "APP-4", "list": ["#7"],
                "body": format!("PR: https://pr/1 for {task}; |${{other}} ${{open") })
        );
        // A manual run has no source to report to: no row.
        run.trigger = Value::Null;
        let step = "  - {id: c, type: action, use: jira.comment, \
                    with: {issue: \"${trigger.issue}\", body: hi}}\n";
        assert!(payloads(&run, step).is_empty());
    }

    /// `(target, params)` of the first step of a workflow with `steps`.
    fn payloads(run: &RunFacts, steps: &str) -> Vec<(String, Value)> {
        let source = format!("name: t\non: [manual]\nsteps:\n{steps}");
        let workflow = sun_workflow::parse(&source).unwrap();
        side_effects(run, &workflow.steps[0])
            .into_iter()
            .map(|item| (item.target, item.payload))
            .collect()
    }

    #[test]
    fn open_pr_gets_the_git_host_contract_from_the_run() {
        let run = facts(true);
        let task_id = run.task.id;
        let got = payloads(&run, "  - {id: pr, type: action, use: github.open_pr}\n");
        assert_eq!(got[0].0, "github.open_pr");
        let body = format!("They are dropped today.\n\n---\nOpened by Sun for task {task_id}.");
        assert_eq!(
            got[0].1,
            json!({
                "repo": "acme/web",
                "head": "sun/MAIN-4-retry-failed-webhooks",
                "base": "trunk",
                "title": "Retry failed webhooks",
                "body": body,
            })
        );
        // `with:` keys win over the run's.
        let step =
            "  - {id: pr, type: action, use: gitea.open_pr, with: {base: rc, draft: true}}\n";
        let got = payloads(&run, step);
        assert_eq!(
            (&got[0].1["base"], &got[0].1["draft"], &got[0].1["repo"]),
            (&json!("rc"), &json!(true), &json!("acme/web"))
        );
        // Without a repo the mod names what is missing.
        let got = payloads(
            &facts(false),
            "  - {id: pr, type: action, use: github.open_pr}\n",
        );
        assert_eq!(got[0].1, Value::Null);
        // Other actions get exactly `with:`.
        let step = "  - {id: c, type: action, use: jira.comment, with: {issue: APP-1, body: hi}}\n";
        assert_eq!(
            payloads(&run, step)[0].1,
            json!({ "issue": "APP-1", "body": "hi" })
        );
    }

    #[test]
    fn notify_sends_mod_slack_params_keyed_per_row() {
        let run = facts(false);
        let step = "  - {id: tell, type: notify, to: [\"slack:#eng\", \"slack:#ops\"], \
                    message: Ready to merge}\n";
        let got = payloads(&run, step);
        let key = format!("{}:tell:notify:slack:#eng", run.run_id);
        assert_eq!(got[0].0, "slack.notify");
        assert_eq!(
            got[0].1,
            json!({ "key": key, "channel": "#eng", "text": "Ready to merge" })
        );
        assert_ne!(got[0].1["key"], got[1].1["key"]);
        let got = payloads(&run, "  - {id: tell, type: notify, to: [\"slack:#eng\"]}\n");
        assert_eq!(got[0].1["text"], "Retry failed webhooks");
    }

    #[tokio::test]
    async fn deliveries_carry_blob_bytes_not_hashes() {
        let dir = tempfile::tempdir().unwrap();
        let store = sun_store::Store::open(dir.path()).await.unwrap();
        let blobs = store.blobs();
        let hash = blobs.put(b"PNG".to_vec()).await.unwrap();
        let file = json!({ "name": "a.png", "blob": hash, "content_type": "image/png" });
        let payload = json!({ "title": "t", "files": [file] });
        let got = inline_blobs(blobs, payload).await.unwrap();
        assert_eq!(
            got,
            json!({ "title": "t", "files": [
                { "name": "a.png", "content_base64": "UE5H", "content_type": "image/png" }
            ] })
        );
        // Payloads without `files` pass through.
        let plain = json!({ "body": "hi" });
        assert_eq!(inline_blobs(blobs, plain.clone()).await.unwrap(), plain);
        // Too many bytes fail the delivery.
        let big = blobs.put(vec![0; MAX_INLINE / 2 + 1]).await.unwrap();
        let two = json!({ "files": [{ "blob": big }, { "blob": big }] });
        assert!(inline_blobs(blobs, two).await.is_err());
    }

    #[test]
    fn repo_slugs_come_from_any_clone_url() {
        for url in [
            "https://github.com/acme/web.git",
            "https://github.com/acme/web/",
            "git@github.com:acme/web.git",
            "ssh://git@github.com/acme/web",
        ] {
            assert_eq!(repo_slug(url), "acme/web", "{url}");
        }
        assert_eq!(repo_slug("https://gitlab.com/g/sub/app.git"), "g/sub/app");
    }
}
