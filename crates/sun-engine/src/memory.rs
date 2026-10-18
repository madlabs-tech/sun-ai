//! Long-term memory (L3) and the learning loop.
//!
//! **Proposals** are `proposed` [`MemoryItem`]s, made when:
//! - the same cause is corrected [`REPEAT`] times in one workflow
//!   ([`on_correction`], in the correction's own transaction);
//! - a human stars a correction or a `learning` note
//!   ([`Engine::star_correction`], [`Engine::star_note`]).
//!
//! **Workspace:** an item belongs to the workspace of the run it was
//! learned from, and only that workspace's runs get it ([`active_for`]).
//!
//! **Scope:** a run in exactly one repo gives a `repo` proposal;
//! otherwise it is `workflow` scope (a correction) or `ecosystem` scope,
//! the whole workspace (a note). Every proposal is an approval card on the Memory screen and, when
//! its run has a repo (a `repo` proposal's, else the run's first),
//! also a PR that adds the line to a file there, pushed from the engine's
//! host and opened by the git host mod's `open_pr` through the
//! **transactional outbox** ([`Engine::open_memory_prs`]).
//!
//! **Target** ([`target_for`]): the file the PR edits is a skill's
//! `SKILL.md` when the correction names a skill of the corrected agent step,
//! the step's workflow prompt file (`.sun/…`) for a `workflow` proposal,
//! else the repo's agent instructions: `AGENTS.md`, or `CLAUDE.md` when the
//! repo has only that. A skill or prompt file the repo doesn't have falls
//! back to the instructions file; the item records the file edited.
//!
//! **Nothing becomes active without a human**: only
//! [`Engine::review_memory`] moves an item, through
//! [`MemoryStatus::can_move`]. Active items reach every later pack of their workspace as P3
//! ([`active_for`]). [`Engine::memory_metrics`] tracks whether mistakes
//! still repeat.

use std::collections::{BTreeMap, HashMap};
use std::time::Duration;

use chrono::Utc;
use serde::Serialize;
use serde_json::json;
use sun_core::host_protocol::{ExecRequest, Worktree};
use sun_core::model::{
    Correction, CorrectionCause, CorrectionKind, EventRecord, MemoryItem, MemoryScope,
    MemoryStatus, NoteKind, OutboxItem, OutboxStatus, Task, WorkflowOwner,
};
use sun_core::{
    CorrectionId, Error, IdemKey, MemoryItemId, NoteId, OutboxId, RepoId, Result, RunId, StepKey,
    TaskId, Timestamp, WorkflowId, WorkspaceId,
};
use sun_store::{
    AgentRepo, CorrectionRepo, EventLog, MemoryRepo, NoteRepo, OutboxRepo, RepoRepo, RunRepo, Tx,
    WorkflowRepo,
};

use sun_workflow::StepKind;

use crate::Engine;
use crate::outbox::repo_slug;
use crate::push;

/// Corrections with the same cause in one workflow that make a proposal.
pub(crate) const REPEAT: u32 = 3;
/// `InvalidInput` when `workflow` is a direct chat's: a chat without an
/// agent writes no memory (BLO-162 §3).
async fn refuse_direct_chat(tx: &mut Tx, workflow: WorkflowId) -> Result<()> {
    if tx.workflow_owner(workflow).await? == WorkflowOwner::DirectChat {
        return Err(Error::InvalidInput(
            "A chat without an agent writes no memory.".into(),
        ));
    }
    Ok(())
}

/// The section learned lines go under, in whichever file the PR edits.
const SECTION: &str = "## Learned from corrections";
/// `author` of proposals Sun makes itself.
const SUN: &str = "sun";
/// How often [`Engine::follow_memory`] opens pending PRs.
const TICK: Duration = Duration::from_secs(30);
/// Longest body, in chars (one line in AGENTS.md and the pack).
const MAX_LINE: usize = 300;
/// Weeks in [`MemoryMetrics::weeks`].
const WEEKS: i64 = 8;

/// Hook: a correction was just recorded (or relabelled) in `tx`. When its
/// cause has now been corrected [`REPEAT`] times in its workflow, proposes a
/// rule against it. While that proposal is open or active nothing more is
/// proposed; once it is rejected or retired, the next proposal waits for
/// [`REPEAT`] more corrections (the mistake keeps recurring).
pub(crate) async fn on_correction(tx: &mut Tx, c: &Correction) -> Result<()> {
    if tx.workflow_owner(c.workflow_id).await? == WorkflowOwner::DirectChat {
        return Ok(());
    }
    let seen = tx.count_corrections(c.workflow_id, c.cause_label).await?;
    let base = format!("repeat:{}:{}", c.workflow_id, c.cause_label);
    let mut key = base.clone();
    let mut round = 1;
    while let Some(status) = tx.memory_status(&key).await? {
        if status != MemoryStatus::Retired {
            return Ok(());
        }
        round += 1;
        key = format!("{base}:{round}");
    }
    if seen < REPEAT * round {
        return Ok(());
    }
    let workflow = tx.get_workflow(c.workflow_id).await?.name;
    let run = tx.get_run(c.run_id).await?;
    let (scope, scope_id) = scope_of(&run.repo_ids, MemoryScope::Workflow, c.workflow_id);
    let item = MemoryItem {
        id: MemoryItemId::generate(),
        workspace_id: tx.workspace_of_run(c.run_id).await?,
        scope,
        scope_id,
        body: rule(c.cause_label, &c.body),
        target: target_for(tx, c.run_id, c.step_key.as_ref(), scope, &c.body).await?,
        reason: format!(
            "{} corrected {seen} times in {workflow}; latest: {}",
            c.cause_label,
            one_line(&c.body, 160)
        ),
        source_run: Some(c.run_id),
        status: MemoryStatus::Proposed,
        author: SUN.into(),
        reviewed_by: None,
        created_at: Utc::now(),
    };
    tx.insert_memory(&item, &key).await?;
    Ok(())
}

/// The classifier hook: relabels correction `id` as `cause` (by
/// `classified_by`, e.g. a model) and re-runs [`on_correction`], so the new
/// cause counts towards a proposal.
pub(crate) async fn relabel(
    tx: &mut Tx,
    id: CorrectionId,
    cause: CorrectionCause,
    classified_by: &str,
) -> Result<()> {
    tx.set_correction_cause(id, cause, classified_by).await?;
    let c = tx.get_correction(id).await?;
    on_correction(tx, &c).await
}

/// The instruction a repeated cause proposes.
fn rule(cause: CorrectionCause, example: &str) -> String {
    use CorrectionCause::*;
    match cause {
        TestFailure => "Run the tests and make them pass before you finish a step.".into(),
        Lint => "Run the linters and type checks and fix every warning before you finish.".into(),
        MissingRequirement => {
            "Check every requirement in the task and its acceptance criteria before you finish."
                .into()
        }
        WrongScope => {
            "Change only what the task asks for; leave unrelated files and code alone.".into()
        }
        Style => "Follow the repo's naming and formatting conventions in every change.".into(),
        Other => format!("Don't repeat this past mistake: {}", one_line(example, 200)),
    }
}

/// `repo` scope when the run worked in exactly one repo, else `fallback`
/// (the workflow for corrections, the ecosystem for notes).
fn scope_of(
    repo_ids: &[RepoId],
    fallback: MemoryScope,
    workflow: WorkflowId,
) -> (MemoryScope, Option<String>) {
    match (repo_ids, fallback) {
        ([repo], _) => (MemoryScope::Repo, Some(repo.to_string())),
        (_, MemoryScope::Workflow) => (MemoryScope::Workflow, Some(workflow.to_string())),
        _ => (MemoryScope::Ecosystem, None),
    }
}

/// The file a proposal from run `run_id` (corrected at `step`, `None`: the
/// whole run) should edit: see the module doc. `None`: the repo's agent
/// instructions file. The agent step blamed is `step` itself or the last
/// agent step before it (a gate checks the work before it).
async fn target_for(
    tx: &mut Tx,
    run_id: RunId,
    step: Option<&StepKey>,
    scope: MemoryScope,
    text: &str,
) -> Result<Option<String>> {
    let run = tx.get_run(run_id).await?;
    let version = tx
        .get_workflow_version(run.workflow_id, run.workflow_version)
        .await?;
    let Ok(workflow) = sun_workflow::parse(&version.source_yaml) else {
        return Ok(None);
    };
    let end = step
        .and_then(|k| workflow.steps.iter().position(|s| &s.id == k))
        .map_or(workflow.steps.len(), |i| i + 1);
    let Some((agent, skills)) = workflow.steps[..end]
        .iter()
        .rev()
        .find_map(|s| match &s.kind {
            StepKind::Agent { skills, .. } => Some((&s.id, skills)),
            _ => None,
        })
    else {
        return Ok(None);
    };
    let text = text.to_lowercase();
    if let Some(skill) = skills
        .iter()
        .find(|s| safe_path(s) && text.contains(&s.to_lowercase()))
    {
        return Ok(Some(format!("{SKILLS_DIR}/{skill}/SKILL.md")));
    }
    if scope != MemoryScope::Workflow {
        return Ok(None);
    }
    // A prompt read from the repo is inlined in the version; the run's
    // `workflow_source` event names its file.
    let source = tx
        .events_after(Some(run_id), 0, u32::MAX)
        .await?
        .into_iter()
        .rfind(|e| e.kind == crate::executor::repo_source::WORKFLOW_SOURCE);
    Ok(source
        .and_then(|e| {
            e.payload["prompts"][agent.as_str()]
                .as_str()
                .map(str::to_owned)
        })
        .filter(|f| safe_path(f)))
}

/// Skills folder of a skill target; [`candidates`] tries every runtime's.
const SKILLS_DIR: &str = ".claude/skills";

/// The files a PR for `target` may edit, best first: a skill target in any
/// runtime's skills folder (`.claude/skills`, `.agents/skills`, …).
fn candidates(target: Option<&str>) -> Vec<String> {
    let Some(target) = target.filter(|t| safe_path(t)) else {
        return Vec::new();
    };
    match target
        .strip_prefix(SKILLS_DIR)
        .and_then(|rest| rest.strip_suffix("/SKILL.md"))
    {
        Some(skill) => sun_core::skill_folders("opencode")
            .iter()
            .map(|dir| format!("{dir}{skill}/SKILL.md"))
            .collect(),
        None => vec![target.to_owned()],
    }
}

/// A relative path of `[A-Za-z0-9._-]` parts without `..`: safe to put in
/// a shell word unquoted.
fn safe_path(path: &str) -> bool {
    path.split('/').all(|part| {
        !part.is_empty()
            && part != ".."
            && part
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
    })
}

/// The first non-empty line of `text`, at most `max` chars.
fn one_line(text: &str, max: usize) -> String {
    let line = text.lines().map(str::trim).find(|l| !l.is_empty());
    let line = line.unwrap_or_default();
    let mut out: String = line.chars().take(max).collect();
    if line.chars().count() > max {
        out.push('…');
    }
    out
}

/// The active items a step of `task` gets as P3, all from the task's
/// workspace: the whole workspace's, the task's workflow's and its latest
/// run's repos'.
pub(crate) async fn active_for(tx: &mut Tx, task: &Task) -> Result<Vec<MemoryItem>> {
    let workflow = task.workflow_id.map(|w| w.to_string());
    let run = tx.runs_of_task(task.id).await?.pop();
    let repos: Vec<String> = run
        .map(|r| r.repo_ids)
        .unwrap_or_default()
        .iter()
        .map(RepoId::to_string)
        .collect();
    Ok(tx
        .active_memory(task.workspace_id)
        .await?
        .into_iter()
        .filter(|m| match (m.scope, m.scope_id.as_ref()) {
            (MemoryScope::Ecosystem, _) => true,
            (MemoryScope::Workflow, Some(id)) => Some(id) == workflow.as_ref(),
            (MemoryScope::Repo, Some(id)) => repos.contains(id),
            _ => false,
        })
        .collect())
}

/// A memory item as the Memory screen shows it.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct MemoryView {
    /// The item.
    #[serde(flatten)]
    pub item: MemoryItem,
    /// What the scope id names: the repo (`owner/name`) or workflow name.
    pub scope_name: Option<String>,
    /// A repo-scope proposal's PR, once enqueued.
    pub pr: Option<PrView>,
}

/// Where a proposal's PR stands.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct PrView {
    /// The `open_pr` outbox row's state.
    pub status: OutboxStatus,
    /// The PR, once opened.
    pub url: Option<String>,
}

/// Corrections and how many repeated an earlier one's cause.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct RepeatRate {
    /// Corrections counted.
    pub corrections: u32,
    /// Those whose cause was corrected before in the same workflow.
    pub repeats: u32,
    /// `repeats / corrections`; `None` without corrections.
    pub rate: Option<f64>,
}

impl RepeatRate {
    fn add(&mut self, repeat: bool) {
        self.corrections += 1;
        self.repeats += u32::from(repeat);
        self.rate = Some(f64::from(self.repeats) / f64::from(self.corrections));
    }
}

/// Gate steps and how many passed their first check.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct PassRate {
    /// Gate steps checked (each step of each run once).
    pub gate_steps: u32,
    /// Those whose first check passed.
    pub first_pass: u32,
    /// `first_pass / gate_steps`; `None` without gate steps.
    pub rate: Option<f64>,
}

impl PassRate {
    fn add(&mut self, pass: bool) {
        self.gate_steps += 1;
        self.first_pass += u32::from(pass);
        self.rate = Some(f64::from(self.first_pass) / f64::from(self.gate_steps));
    }
}

/// One week of [`MemoryMetrics::weeks`] (or of [`FirstPass::weeks`]).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct WeekRate<R = RepeatRate> {
    /// Start of the 7-day window.
    pub week_start: Timestamp,
    /// The window's rate.
    #[serde(flatten)]
    pub rate: R,
}

/// The rate of one workflow or runtime.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct NamedRate<R = RepeatRate> {
    /// Workflow or runtime name.
    pub name: String,
    /// Its rate.
    #[serde(flatten)]
    pub rate: R,
}

/// First-pass gate rate: overall, the last 8 weeks (oldest
/// first), per workflow and per runtime (most gate steps first).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct FirstPass {
    /// Every gate step.
    pub overall: PassRate,
    /// By the week of the first check.
    pub weeks: Vec<WeekRate<PassRate>>,
    /// Per workflow.
    pub by_workflow: Vec<NamedRate<PassRate>>,
    /// Per runtime of the agent step that ran last before the first check
    /// (`none`: no agent ran).
    pub by_runtime: Vec<NamedRate<PassRate>>,
}

/// Review rejections over time: `reject` corrections (a rejected
/// approval or a PR change request), each rate's `corrections` counting
/// them and `repeats` those whose cause was rejected before in the workflow.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ReviewRejections {
    /// Every rejection.
    pub overall: RepeatRate,
    /// The last 8 weeks, oldest first.
    pub weeks: Vec<WeekRate>,
    /// Per workflow, most rejections first.
    pub by_workflow: Vec<NamedRate>,
    /// Per runtime, most rejections first.
    pub by_runtime: Vec<NamedRate>,
}

/// Are agents improving? A correction *repeats* when its cause was
/// corrected before in the same workflow.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct MemoryMetrics {
    /// Every correction.
    pub overall: RepeatRate,
    /// The last 8 weeks, oldest first.
    pub weeks: Vec<WeekRate>,
    /// Per workflow, most corrections first.
    pub by_workflow: Vec<NamedRate>,
    /// Per runtime (`none`: no agent ran), most corrections first.
    pub by_runtime: Vec<NamedRate>,
    /// Active memory items.
    pub active_items: u32,
    /// Proposals waiting for review.
    pub proposed_items: u32,
    /// Do gates pass the first time?
    pub first_pass: FirstPass,
    /// Do reviewers still reject the work?
    pub review_rejections: ReviewRejections,
}

/// The 8 empty weekly windows ending at `now`, oldest first.
fn week_windows<R: Default>(now: Timestamp) -> Vec<WeekRate<R>> {
    (0..WEEKS)
        .rev()
        .map(|i| WeekRate {
            week_start: now - chrono::Duration::days(7 * (i + 1)),
            rate: R::default(),
        })
        .collect()
}

/// The window index of `at`, if within the last 8 weeks.
fn week_of(now: Timestamp, at: Timestamp) -> Option<usize> {
    let age = (now - at).num_days();
    (0..7 * WEEKS)
        .contains(&age)
        .then(|| usize::try_from(WEEKS - 1 - age / 7).unwrap_or_default())
}

/// First-pass gate rates from `gate_result` events (any order): a gate step
/// of a run passes first time when every result of its lowest attempt
/// passed (a `waiting` merge-order check counts as passing). Workflows are
/// named by `workflow_of` (the run's); runtimes by `runtime_of` (the one
/// whose work the first check at that time checked).
pub(crate) fn first_pass_metrics(
    events: &[EventRecord],
    now: Timestamp,
    workflow_of: impl Fn(RunId) -> String,
    runtime_of: impl Fn(RunId, Timestamp) -> String,
) -> FirstPass {
    // (run, step) -> (lowest attempt, its first time, all passed at it)
    let mut first: BTreeMap<(RunId, String), (u64, Timestamp, bool)> = BTreeMap::new();
    for e in events {
        let Some(step) = &e.step_key else { continue };
        let attempt = e.payload["attempt"].as_u64().unwrap_or(1);
        let result = &e.payload["result"];
        let pass = result["pass"] == true || result["waiting"] == true;
        let entry = first
            .entry((e.run_id, step.to_string()))
            .or_insert((attempt, e.at, true));
        if attempt < entry.0 {
            *entry = (attempt, e.at, true);
        }
        if attempt == entry.0 {
            entry.1 = entry.1.min(e.at);
            entry.2 &= pass;
        }
    }
    let mut overall = PassRate::default();
    let mut weeks: Vec<WeekRate<PassRate>> = week_windows(now);
    let mut by_workflow: BTreeMap<String, PassRate> = BTreeMap::new();
    let mut by_runtime: BTreeMap<String, PassRate> = BTreeMap::new();
    for ((run, _), (_, at, pass)) in first {
        overall.add(pass);
        if let Some(i) = week_of(now, at) {
            weeks[i].rate.add(pass);
        }
        by_workflow.entry(workflow_of(run)).or_default().add(pass);
        by_runtime.entry(runtime_of(run, at)).or_default().add(pass);
    }
    let named = |rows: BTreeMap<String, PassRate>| {
        let mut rows: Vec<NamedRate<PassRate>> = rows
            .into_iter()
            .map(|(name, rate)| NamedRate { name, rate })
            .collect();
        rows.sort_by(|a, b| b.rate.gate_steps.cmp(&a.rate.gate_steps));
        rows
    };
    FirstPass {
        overall,
        weeks,
        by_workflow: named(by_workflow),
        by_runtime: named(by_runtime),
    }
}

/// The repeat rates of `corrections` (any order), with weeks ending at
/// `now`; workflows are named by `workflow_name`.
pub(crate) fn repeat_metrics(
    corrections: &[Correction],
    now: Timestamp,
    workflow_name: impl Fn(WorkflowId) -> String,
) -> (RepeatRate, Vec<WeekRate>, Vec<NamedRate>, Vec<NamedRate>) {
    let mut sorted: Vec<&Correction> = corrections.iter().collect();
    sorted.sort_by_key(|c| (c.created_at, c.id));
    let mut overall = RepeatRate::default();
    let mut weeks: Vec<WeekRate> = week_windows(now);
    let mut by_workflow: BTreeMap<WorkflowId, RepeatRate> = BTreeMap::new();
    let mut by_runtime: BTreeMap<String, RepeatRate> = BTreeMap::new();
    let mut seen = std::collections::HashSet::new();
    for c in sorted {
        let repeat = !seen.insert((c.workflow_id, c.cause_label));
        overall.add(repeat);
        if let Some(i) = week_of(now, c.created_at) {
            weeks[i].rate.add(repeat);
        }
        by_workflow.entry(c.workflow_id).or_default().add(repeat);
        let runtime = c.runtime.clone().unwrap_or_else(|| "none".into());
        by_runtime.entry(runtime).or_default().add(repeat);
    }
    let named = |rows: Vec<(String, RepeatRate)>| {
        let mut rows: Vec<NamedRate> = rows
            .into_iter()
            .map(|(name, rate)| NamedRate { name, rate })
            .collect();
        rows.sort_by(|a, b| b.rate.corrections.cmp(&a.rate.corrections));
        rows
    };
    let by_workflow = by_workflow
        .into_iter()
        .map(|(id, rate)| (workflow_name(id), rate))
        .collect();
    (
        overall,
        weeks,
        named(by_workflow),
        named(by_runtime.into_iter().collect()),
    )
}

/// The git host mod of a clone URL.
// ponytail: by host name; read it from the repo row once repos name their mod.
pub(crate) fn git_mod(url: &str) -> &'static str {
    if url.contains("gitlab") {
        "gitlab"
    } else {
        "github"
    }
}

/// The edit script's last line names the file it edited.
const EDITED: &str = "sun-memory-file=";

/// On branch `branch`, appends `$SUN_MEMORY_LINE` under [`SECTION`] of the
/// first of `$SUN_MEMORY_FILES` (safe paths) the repo has, else of
/// `AGENTS.md` (`CLAUDE.md` when the repo has only that), once (a retry
/// finds it there). Prints [`EDITED`] and the file.
fn edit_script(branch: &str) -> String {
    format!(
        "git checkout -q -B {branch} || exit 1\n\
         f=AGENTS.md; [ -f AGENTS.md ] || [ ! -f CLAUDE.md ] || f=CLAUDE.md\n\
         for c in $SUN_MEMORY_FILES; do [ -f \"$c\" ] && {{ f=\"$c\"; break; }}; done\n\
         touch \"$f\" || exit 1\n\
         grep -qxF '{SECTION}' \"$f\" || printf '\\n{SECTION}\\n\\n' >> \"$f\"\n\
         grep -qxF -- \"- $SUN_MEMORY_LINE\" \"$f\" || printf -- '- %s\\n' \"$SUN_MEMORY_LINE\" >> \"$f\"\n\
         printf '{EDITED}%s\\n' \"$f\"\n",
        branch = crate::gates::quote(branch),
    )
}

impl Engine {
    /// The memory items of `workspace` (`None`: of every workspace), newest
    /// first.
    pub async fn memory(&self, workspace: Option<WorkspaceId>) -> Result<Vec<MemoryView>> {
        let mut tx = self.store.read().await?;
        let mut names: HashMap<String, Option<String>> = HashMap::new();
        let mut out = Vec::new();
        for (item, pr) in tx.memory_items(workspace).await? {
            let scope_name = match (item.scope, item.scope_id.clone()) {
                (MemoryScope::Ecosystem, _) | (_, None) => None,
                (scope, Some(id)) => {
                    if !names.contains_key(&id) {
                        let name = match scope {
                            MemoryScope::Repo => match id.parse() {
                                Ok(repo) => tx.get_repo(repo).await.ok().map(|r| repo_slug(&r.url)),
                                Err(_) => None,
                            },
                            _ => match id.parse() {
                                Ok(workflow) => {
                                    tx.get_workflow(workflow).await.ok().map(|w| w.name)
                                }
                                Err(_) => None,
                            },
                        };
                        names.insert(id.clone(), name);
                    }
                    names[&id].clone()
                }
            };
            let pr = pr.map(|pr| PrView {
                status: pr.status,
                url: pr.url,
            });
            out.push(MemoryView {
                item,
                scope_name,
                pr,
            });
        }
        Ok(out)
    }

    /// A human approves (`active`), rejects or retires (`retired`) item `id`.
    pub async fn review_memory(
        &self,
        id: MemoryItemId,
        to: MemoryStatus,
        by: &str,
    ) -> Result<MemoryItem> {
        let mut tx = self.store.begin().await?;
        let item = tx.review_memory(id, to, by).await?;
        tx.commit().await?;
        Ok(item)
    }

    /// Relabels correction `id` (a classifier, after commit); see [`relabel`].
    pub async fn relabel_correction(
        &self,
        id: CorrectionId,
        cause: CorrectionCause,
        classified_by: &str,
    ) -> Result<()> {
        let mut tx = self.store.begin().await?;
        relabel(&mut tx, id, cause, classified_by).await?;
        tx.commit().await?;
        Ok(())
    }

    /// A human stars correction `id`: proposes not repeating it. `false`
    /// when it was starred before.
    pub async fn star_correction(&self, id: CorrectionId, by: &str) -> Result<bool> {
        let mut tx = self.store.begin().await?;
        let c = tx.get_correction(id).await?;
        refuse_direct_chat(&mut tx, c.workflow_id).await?;
        let run = tx.get_run(c.run_id).await?;
        let (scope, scope_id) = scope_of(&run.repo_ids, MemoryScope::Workflow, c.workflow_id);
        let target = target_for(&mut tx, c.run_id, c.step_key.as_ref(), scope, &c.body).await?;
        let item = MemoryItem {
            id: MemoryItemId::generate(),
            workspace_id: tx.workspace_of_run(c.run_id).await?,
            scope,
            scope_id,
            body: rule(CorrectionCause::Other, &c.body),
            target,
            reason: format!("{} ({}) starred by {by}", c.kind, c.cause_label),
            source_run: Some(c.run_id),
            status: MemoryStatus::Proposed,
            author: by.to_owned(),
            reviewed_by: None,
            created_at: Utc::now(),
        };
        let created = tx
            .insert_memory(&item, &format!("star:correction:{id}"))
            .await?;
        tx.commit().await?;
        Ok(created)
    }

    /// A human stars `learning` note `id`: proposes it as memory. `false`
    /// when it was starred before.
    pub async fn star_note(&self, id: NoteId, by: &str) -> Result<bool> {
        let mut tx = self.store.begin().await?;
        let note = tx.get_note(id).await?;
        if note.kind != NoteKind::Learning {
            return Err(Error::InvalidInput(format!(
                "only learning notes become memory; this one is a {}",
                note.kind
            )));
        }
        let run = tx.get_run(note.run_id).await?;
        refuse_direct_chat(&mut tx, run.workflow_id).await?;
        let (scope, scope_id) = scope_of(&run.repo_ids, MemoryScope::Ecosystem, run.workflow_id);
        let step = Some(&note.step_key);
        let target = target_for(&mut tx, note.run_id, step, scope, &note.body).await?;
        let item = MemoryItem {
            id: MemoryItemId::generate(),
            workspace_id: tx.workspace_of_run(note.run_id).await?,
            scope,
            scope_id,
            body: one_line(&note.body, MAX_LINE),
            target,
            reason: format!("learning note from step {} starred by {by}", note.step_key),
            source_run: Some(note.run_id),
            status: MemoryStatus::Proposed,
            author: by.to_owned(),
            reviewed_by: None,
            created_at: Utc::now(),
        };
        let created = tx.insert_memory(&item, &format!("star:note:{id}")).await?;
        tx.commit().await?;
        Ok(created)
    }

    /// Repeat-mistake rate over time, per workflow and runtime, of the runs
    /// on `workspace` (`None`: on every workspace).
    // ponytail: computed in memory from every correction, one workspace
    // lookup per run; aggregate in SQL when corrections reach the hundreds
    // of thousands.
    pub async fn memory_metrics(&self, workspace: Option<WorkspaceId>) -> Result<MemoryMetrics> {
        let mut tx = self.store.read().await?;
        let mut on: HashMap<RunId, bool> = HashMap::new();
        let mut corrections = tx.corrections(u32::MAX).await?;
        let mut gates = tx.events_of_kind("gate_result").await?;
        if let Some(workspace) = workspace {
            let runs = corrections.iter().map(|c| c.run_id);
            for run in runs
                .chain(gates.iter().map(|e| e.run_id))
                .collect::<Vec<_>>()
            {
                if let std::collections::hash_map::Entry::Vacant(slot) = on.entry(run) {
                    slot.insert(tx.workspace_of_run(run).await.ok() == Some(workspace));
                }
            }
            corrections.retain(|c| on[&c.run_id]);
            gates.retain(|e| on[&e.run_id]);
        }
        let mut names = HashMap::new();
        for c in &corrections {
            if let std::collections::hash_map::Entry::Vacant(e) = names.entry(c.workflow_id) {
                let name = tx.get_workflow(c.workflow_id).await.map(|w| w.name);
                e.insert(name.unwrap_or_else(|_| c.workflow_id.to_string()));
            }
        }
        let items = tx.memory_items(workspace).await?;
        let count = |status| {
            let n = items.iter().filter(|(m, _)| m.status == status).count();
            u32::try_from(n).unwrap_or(u32::MAX)
        };
        let (overall, weeks, by_workflow, by_runtime) =
            repeat_metrics(&corrections, Utc::now(), |id| names[&id].clone());
        let rejects: Vec<Correction> = corrections
            .iter()
            .filter(|c| c.kind == CorrectionKind::Reject)
            .cloned()
            .collect();
        let rejected = repeat_metrics(&rejects, Utc::now(), |id| names[&id].clone());
        let mut run_workflow = HashMap::new();
        // Per run: (start, runtime name) of each agent attempt.
        let mut run_agents: HashMap<RunId, Vec<(Timestamp, String)>> = HashMap::new();
        let mut runtime_names = HashMap::new();
        for e in &gates {
            if let std::collections::hash_map::Entry::Vacant(slot) = run_workflow.entry(e.run_id) {
                let workflow = tx.get_run(e.run_id).await?.workflow_id;
                let name = tx.get_workflow(workflow).await.map(|w| w.name);
                slot.insert(name.unwrap_or_else(|_| workflow.to_string()));
                let mut agents = Vec::new();
                for s in tx.steps_of_run(e.run_id).await? {
                    let id = match (s.credential_id, s.runtime_id) {
                        (Some(c), _) => c.to_string(),
                        (None, Some(r)) => r.to_string(),
                        (None, None) => continue,
                    };
                    let Some(at) = s.started_at else {
                        continue;
                    };
                    if let std::collections::hash_map::Entry::Vacant(slot) =
                        runtime_names.entry(id.clone())
                    {
                        let name = crate::executor::ran_on(&mut tx, &s).await;
                        let name = name.ok().flatten().map(|(name, _)| name);
                        slot.insert(name.unwrap_or_else(|| id.clone()));
                    }
                    agents.push((at, runtime_names[&id].clone()));
                }
                run_agents.insert(e.run_id, agents);
            }
        }
        let runtime_of = |run: RunId, at: Timestamp| {
            run_agents[&run]
                .iter()
                .filter(|(started, _)| *started <= at)
                .max_by_key(|(started, _)| *started)
                .map_or_else(|| "none".to_owned(), |(_, name)| name.clone())
        };
        let first_pass = first_pass_metrics(
            &gates,
            Utc::now(),
            |run| run_workflow[&run].clone(),
            runtime_of,
        );
        Ok(MemoryMetrics {
            overall,
            weeks,
            by_workflow,
            by_runtime,
            active_items: count(MemoryStatus::Active),
            proposed_items: count(MemoryStatus::Proposed),
            first_pass,
            review_rejections: ReviewRejections {
                overall: rejected.0,
                weeks: rejected.1,
                by_workflow: rejected.2,
                by_runtime: rejected.3,
            },
        })
    }

    /// Opens the PR of every proposal that has none yet and a repo to open
    /// it in (see the module doc): adds its line to its target file on
    /// branch `sun/<item id>`, pushes it from the engine's host and enqueues
    /// the git host mod's `open_pr`. Returns how many were enqueued; one
    /// that fails waits for the next call.
    pub async fn open_memory_prs(&self) -> Result<usize> {
        let items = self.store.read().await?.memory_prs_to_open().await?;
        let mut opened = 0;
        for item in items {
            match self.open_memory_pr(&item).await {
                Ok(enqueued) => opened += usize::from(enqueued),
                Err(e) => eprintln!("sun: memory: PR for {}: {e}", item.id),
            }
        }
        Ok(opened)
    }

    /// The repo a proposal's PR goes to: a `repo` proposal's, else its run's
    /// first repo. `None`: a repo-less run (an approval card only).
    async fn memory_repo(&self, item: &MemoryItem, run_id: RunId) -> Result<Option<RepoId>> {
        if let (MemoryScope::Repo, Some(id)) = (item.scope, item.scope_id.as_deref()) {
            return Ok(Some(id.parse()?));
        }
        let mut tx = self.store.read().await?;
        Ok(tx.get_run(run_id).await?.repo_ids.first().copied())
    }

    // ponytail: a repo-less proposal is looked at again every tick until it
    // is reviewed; mark it once proposals pile up.
    async fn open_memory_pr(&self, item: &MemoryItem) -> Result<bool> {
        let Some(run_id) = item.source_run else {
            return Ok(false);
        };
        let Some(repo_id) = self.memory_repo(item, run_id).await? else {
            return Ok(false);
        };
        let repo = self.store.read().await?.get_repo(repo_id).await?;
        let mod_name = git_mod(&repo.url);
        // The host cuts `sun/<id>` for a worktree; the item's id names the PR branch.
        let task_id: TaskId = item.id.to_string().parse()?;
        let branch = format!("sun/{task_id}");
        let worktree = Worktree::Repo {
            task_id,
            url: repo.url.clone(),
            base: Some(repo.default_branch.clone()),
            branch: branch.clone(),
        };
        let cwd = self
            .hosts
            .prepare_worktree(self.host, worktree.clone())
            .await?;
        let env = [
            (
                "SUN_MEMORY_FILES".to_owned(),
                candidates(item.target.as_deref()).join(" "),
            ),
            ("SUN_MEMORY_LINE".to_owned(), one_line(&item.body, MAX_LINE)),
        ];
        let request = ExecRequest {
            cwd,
            command: edit_script(&branch),
            env: env.into_iter().collect(),
            timeout_s: 60,
        };
        let out = self.hosts.exec(self.host, request).await?;
        let file = out
            .output
            .lines()
            .rev()
            .find_map(|l| l.strip_prefix(EDITED))
            .map(str::to_owned);
        let (Some(0), Some(file)) = (out.code, file) else {
            return Err(Error::Unavailable(format!(
                "edit the memory file: {}",
                out.output.trim()
            )));
        };
        let message = format!("docs: learn \"{}\"", one_line(&item.body, 60));
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

        let key = IdemKey::new(run_id, &StepKey::new("memory")?, &format!("pr:{}", item.id));
        let body = format!(
            "Sun proposes adding this line to `{file}` (section \"{SECTION}\"):\n\n\
             > {}\n\nWhy: {}\n\nMerge to apply it, or close the PR to reject it. \
             Sun memory item `{}`.",
            item.body, item.reason, item.id
        );
        let row = OutboxItem {
            id: OutboxId::generate(),
            run_id: Some(run_id),
            idem_key: key.clone(),
            target: format!("{mod_name}.open_pr"),
            payload: json!({
                "repo": repo_slug(&repo.url),
                "head": branch,
                "base": repo.default_branch,
                "title": format!("Sun memory: {}", one_line(&item.body, 60)),
                "body": body,
            }),
            status: OutboxStatus::Pending,
            result: None,
        };
        let mut tx = self.store.begin().await?;
        tx.enqueue(&row).await?;
        tx.set_memory_pr(item.id, &key, &file).await?;
        tx.commit().await?;
        self.outbox_wake.notify_one();
        Ok(true)
    }

    /// Runs forever: opens pending memory PRs every 30 s. `sun serve`
    /// spawns it.
    pub async fn follow_memory(self) {
        let mut tick = tokio::time::interval(TICK);
        loop {
            tick.tick().await;
            if let Err(e) = self.open_memory_prs().await {
                eprintln!("sun: memory: {e}");
            }
        }
    }
}

#[cfg(test)]
mod tests;
