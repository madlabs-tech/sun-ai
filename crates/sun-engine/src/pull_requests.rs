//! Pull requests mirrored from the git hosts, part of the **Facade** for the
//! API: webhook intake, the PR auto-link rule and the PR auto-complete
//! decision.
//!
//! A `pull_request` delivery (GitHub) or Merge Request Hook (GitLab) becomes
//! a [`PrEvent`], and [`mirror`] applies it in one transaction: upsert the
//! [`PullRequest`] row (a stale delivery writes nothing), make its automatic
//! links match the task keys it claims ([`reconcile_auto_links`]), and run
//! the merge rule ([`maybe_auto_complete`]) for every task the event touched.
//!
//! **The merge rule (decision 6):** once every PR linked to a task has
//! merged, the task moves on its own: to `done` when its latest run was a
//! workflow's, to `needs_you` when an agent did the work (or nothing ran),
//! unless the workspace turned "move tasks when their PRs merge" off or a
//! person turned it off for that task. The rule only runs on a PR event (a
//! merge, a link, an unlink), never on a settings change, so a reopened
//! task stays open until new work lands.
//!
//! Checks and mergeability are a separate snapshot
//! ([`Engine::refresh_snapshot`]): one GraphQL call after a delivery, stored
//! only while the head it describes is still the PR's head.

use std::sync::LazyLock;

use regex::Regex;
use serde::Serialize;
use serde_json::{Value, json};
use sun_core::model::{
    ActivityKind, GitSettings, PrSnapshot, PrState, PullRequest, Task, TaskKey, TaskPullRequest,
    VcsConnection, VcsProvider, Workspace,
};
use sun_core::{
    Error, MemberId, PullRequestId, Result, TaskId, TaskStatus, Timestamp, WorkspaceId,
};
use sun_store::{AgentRepo, PullRequestRepo, RunRepo, TaskRepo, Tx, VcsRepo, WorkspaceRepo};

use crate::Engine;

/// A task key anywhere in a title or branch: `SUN-12`, `sun-12`. The left
/// word boundary keeps `abc@SUN-1` out, the right one `v1.2-3`.
static KEY: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\b([a-z][a-z0-9]{0,9})-(\d+)\b").unwrap_or_else(|e| panic!("{e}"))
});

/// A task key right after a closing keyword (`Closes SUN-12`, `fix: sun-12`),
/// the one way a body links a task; a bare mention there links nothing.
static CLOSING_KEY: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\b(?:close[sd]?|fix(?:e[sd])?|resolve[sd]?)[:\s]+([a-z][a-z0-9]{0,9})-(\d+)\b")
        .unwrap_or_else(|e| panic!("{e}"))
});

/// `scheme://host/owner/repo/(pull|pulls|merge_requests)/N`; anything after
/// (`/files`, `?tab`, `#issue`) is dropped.
static PR_URL: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)^(https?://[^/?#]+/.+?/(?:pull|pulls|merge_requests)/\d+)(?:[/?#].*)?$")
        .unwrap_or_else(|e| panic!("{e}"))
});

/// The keys `re` finds in `text`, in order, without repeats.
fn keys_in(re: &Regex, text: &str, out: &mut Vec<TaskKey>) {
    for found in re.captures_iter(text) {
        let (Some(prefix), Some(number)) = (found.get(1), found.get(2)) else {
            continue;
        };
        if let Ok(key) = format!("{}-{}", prefix.as_str(), number.as_str()).parse::<TaskKey>()
            && !out.contains(&key)
        {
            out.push(key);
        }
    }
}

/// The task keys a PR claims: any key in its title or branch name, and a
/// key after a closing keyword in its title or body. Uppercase, in order,
/// without repeats.
pub(crate) fn claimed_keys(title: &str, body: &str, branch: &str) -> Vec<TaskKey> {
    let mut keys = Vec::new();
    keys_in(&KEY, title, &mut keys);
    keys_in(&KEY, branch, &mut keys);
    keys_in(&CLOSING_KEY, title, &mut keys);
    keys_in(&CLOSING_KEY, body, &mut keys);
    keys
}

/// A pasted PR URL as the host stores it: trimmed, `https://` when no
/// scheme, scheme and host lowercased, cut after the number. `None` when it
/// isn't a PR or MR page.
pub(crate) fn normalize_pr_url(raw: &str) -> Option<String> {
    let raw = raw.trim();
    let raw = match raw.contains("://") {
        true => raw.to_owned(),
        false => format!("https://{raw}"),
    };
    let found = PR_URL.captures(&raw)?.get(1)?.as_str();
    let (scheme, rest) = found.split_once("://")?;
    let (host, path) = rest.split_once('/')?;
    if host.is_empty() {
        return None;
    }
    Some(format!(
        "{}://{}/{}",
        scheme.to_lowercase(),
        host.to_lowercase(),
        path.trim_end_matches('/')
    ))
}

/// A PR as one git host delivery describes it; see [`PrEvent::from_github`]
/// and [`PrEvent::from_gitlab`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrEvent {
    /// Which host sent it.
    pub provider: VcsProvider,
    /// GitHub: the App installation it came from.
    pub installation_id: Option<i64>,
    /// Repository owner (GitLab: the full namespace).
    pub repo_owner: String,
    /// Repository name.
    pub repo_name: String,
    /// PR number (GitLab: the MR `iid`).
    pub number: i64,
    /// Title.
    pub title: String,
    /// Description; only read for closing keywords, never stored.
    pub body: String,
    /// State.
    pub state: PrState,
    /// Link to it.
    pub html_url: String,
    /// Head branch.
    pub branch: Option<String>,
    /// Author's login.
    pub author_login: Option<String>,
    /// Author's avatar.
    pub author_avatar_url: Option<String>,
    /// When it merged.
    pub merged_at: Option<Timestamp>,
    /// When it closed.
    pub closed_at: Option<Timestamp>,
    /// When the host says it was created (now when it didn't say).
    pub created_at: Timestamp,
    /// When the host says it last changed (now when it didn't say).
    pub updated_at: Timestamp,
    /// Head commit.
    pub head_sha: String,
    /// Lines added.
    pub additions: i64,
    /// Lines deleted.
    pub deletions: i64,
    /// Files changed.
    pub changed_files: i64,
}

/// A host timestamp: RFC 3339, or GitLab's `2017-09-20 08:31:45 UTC`.
fn time(value: &Value) -> Option<Timestamp> {
    let text = value.as_str()?;
    if let Ok(at) = chrono::DateTime::parse_from_rfc3339(text) {
        return Some(at.to_utc());
    }
    ["%Y-%m-%d %H:%M:%S UTC", "%Y-%m-%d %H:%M:%S%.f UTC"]
        .iter()
        .find_map(|layout| chrono::NaiveDateTime::parse_from_str(text, layout).ok())
        .map(|naive| naive.and_utc())
        .or_else(|| {
            chrono::DateTime::parse_from_str(text, "%Y-%m-%d %H:%M:%S %z")
                .ok()
                .map(|at| at.to_utc())
        })
}

fn text(value: &Value) -> Option<String> {
    value.as_str().filter(|s| !s.is_empty()).map(str::to_owned)
}

impl PrEvent {
    /// A GitHub `pull_request` delivery; `None` when it names no PR or
    /// repository. State: merged, else closed, else draft, else open.
    pub fn from_github(payload: &Value) -> Option<Self> {
        let pr = &payload["pull_request"];
        let repo = &payload["repository"];
        let state = if pr["merged"].as_bool().unwrap_or(false) {
            PrState::Merged
        } else if pr["state"].as_str() == Some("closed") {
            PrState::Closed
        } else if pr["draft"].as_bool().unwrap_or(false) {
            PrState::Draft
        } else {
            PrState::Open
        };
        let now = chrono::Utc::now();
        Some(Self {
            provider: VcsProvider::Github,
            installation_id: payload["installation"]["id"].as_i64(),
            repo_owner: text(&repo["owner"]["login"])?,
            repo_name: text(&repo["name"])?,
            number: pr["number"].as_i64()?,
            title: pr["title"].as_str().unwrap_or_default().to_owned(),
            body: pr["body"].as_str().unwrap_or_default().to_owned(),
            state,
            html_url: text(&pr["html_url"])?,
            branch: text(&pr["head"]["ref"]),
            author_login: text(&pr["user"]["login"]),
            author_avatar_url: text(&pr["user"]["avatar_url"]),
            merged_at: time(&pr["merged_at"]),
            closed_at: time(&pr["closed_at"]),
            created_at: time(&pr["created_at"]).unwrap_or(now),
            updated_at: time(&pr["updated_at"]).unwrap_or(now),
            head_sha: pr["head"]["sha"].as_str().unwrap_or_default().to_owned(),
            additions: pr["additions"].as_i64().unwrap_or(0),
            deletions: pr["deletions"].as_i64().unwrap_or(0),
            changed_files: pr["changed_files"].as_i64().unwrap_or(0),
        })
    }

    /// A GitLab Merge Request Hook; `None` when it names no MR or project.
    /// `path_with_namespace` splits on its last `/` (subgroups stay in the
    /// owner); `locked` reads as open; a draft is `draft`,
    /// `work_in_progress` or a title starting with `draft:`.
    pub fn from_gitlab(payload: &Value) -> Option<Self> {
        let mr = &payload["object_attributes"];
        let (owner, name) = split_namespace(payload["project"]["path_with_namespace"].as_str()?)?;
        let title = mr["title"].as_str().unwrap_or_default().to_owned();
        let draft = mr["draft"].as_bool().unwrap_or(false)
            || mr["work_in_progress"].as_bool().unwrap_or(false)
            || title.to_lowercase().starts_with("draft:");
        let state = match mr["state"].as_str() {
            Some("merged") => PrState::Merged,
            Some("closed") => PrState::Closed,
            _ if draft => PrState::Draft,
            _ => PrState::Open,
        };
        let now = chrono::Utc::now();
        Some(Self {
            provider: VcsProvider::Gitlab,
            installation_id: None,
            repo_owner: owner,
            repo_name: name,
            number: mr["iid"].as_i64()?,
            title,
            body: mr["description"].as_str().unwrap_or_default().to_owned(),
            state,
            html_url: text(&mr["url"])?,
            branch: text(&mr["source_branch"]),
            author_login: text(&payload["user"]["username"]),
            author_avatar_url: text(&payload["user"]["avatar_url"]),
            merged_at: (state == PrState::Merged).then_some(now),
            closed_at: (state == PrState::Closed).then_some(now),
            created_at: time(&mr["created_at"]).unwrap_or(now),
            updated_at: time(&mr["updated_at"]).unwrap_or(now),
            head_sha: mr["last_commit"]["id"]
                .as_str()
                .unwrap_or_default()
                .to_owned(),
            additions: 0,
            deletions: 0,
            changed_files: 0,
        })
    }

    /// The row to upsert for `workspace`; the snapshot columns are empty
    /// (the store never writes them from here).
    fn pull_request(&self, workspace: WorkspaceId) -> PullRequest {
        PullRequest {
            id: PullRequestId::generate(),
            workspace_id: workspace,
            provider: self.provider,
            installation_id: self.installation_id,
            repo_owner: self.repo_owner.clone(),
            repo_name: self.repo_name.clone(),
            number: self.number,
            title: self.title.clone(),
            state: self.state,
            html_url: self.html_url.clone(),
            branch: self.branch.clone(),
            author_login: self.author_login.clone(),
            author_avatar_url: self.author_avatar_url.clone(),
            merged_at: self.merged_at,
            closed_at: self.closed_at,
            pr_created_at: self.created_at,
            pr_updated_at: self.updated_at,
            head_sha: self.head_sha.clone(),
            additions: self.additions,
            deletions: self.deletions,
            changed_files: self.changed_files,
            mergeable: None,
            merge_state_status: None,
            checks_rollup_state: None,
            checks_total: 0,
            checks_passed: 0,
            checks_failed: 0,
            checks_pending: 0,
            snapshot_head_sha: String::new(),
            snapshot_fetched_at: None,
        }
    }
}

/// `group/subgroup/repo` → (`group/subgroup`, `repo`).
fn split_namespace(path: &str) -> Option<(String, String)> {
    let path = path.trim_matches('/');
    let (owner, name) = path.rsplit_once('/')?;
    (!owner.is_empty() && !name.is_empty()).then(|| (owner.to_owned(), name.to_owned()))
}

/// How a PR came to be on a task.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LinkSource {
    /// A person linked it.
    Manual,
    /// Its title names the task.
    Title,
    /// Its branch names the task.
    Branch,
    /// A closing keyword in its body did, or its text changed since.
    Auto,
}

/// The link's source, given the task's key.
fn link_source(link: &TaskPullRequest, pr: &PullRequest, key: &TaskKey) -> LinkSource {
    if link.linked_by.is_some() {
        return LinkSource::Manual;
    }
    let names = |text: &str| {
        let mut keys = Vec::new();
        keys_in(&KEY, text, &mut keys);
        keys.contains(key)
    };
    if names(&pr.title) {
        LinkSource::Title
    } else if names(pr.branch.as_deref().unwrap_or_default()) {
        LinkSource::Branch
    } else {
        LinkSource::Auto
    }
}

/// Where the merge rule stands for one task ([`AutoComplete::state`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AutoCompleteState {
    /// No linked PR.
    None,
    /// The task is not open any more.
    Terminal,
    /// The workspace leaves tasks alone when PRs merge.
    WorkspaceDisabled,
    /// A person turned the rule off for this task.
    TaskDisabled,
    /// Already at the target status.
    AtTarget,
    /// Some PRs are still open or draft.
    Waiting,
    /// Some PRs closed without merging.
    NotMerged,
    /// Every linked PR merged.
    AllMerged,
}

/// The merge rule's decision for a task, as the task page shows it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AutoComplete {
    /// Where the rule stands.
    pub state: AutoCompleteState,
    /// The PRs the state is about: the open ones for `waiting`, the
    /// closed ones for `not_merged`, all of them for `all_merged`.
    pub pull_request_ids: Vec<PullRequestId>,
    /// A person turned the rule off for this task.
    pub task_disabled: bool,
    /// The workspace moves tasks when their PRs merge at all.
    pub workspace_enabled: bool,
    /// Where a merge moves this task; `None` when the workspace doesn't.
    pub target_status: Option<TaskStatus>,
}

/// The decision for `task` with its linked `prs`, guards checked in this
/// order: none → terminal → workspace_disabled →
/// task_disabled → at_target → waiting → not_merged → all_merged. `target`
/// is where a merge would move it ([`target_of`]).
pub(crate) fn decide(
    task: &Task,
    prs: &[PullRequest],
    settings: GitSettings,
    target: TaskStatus,
) -> AutoComplete {
    use AutoCompleteState::*;
    let mut decision = AutoComplete {
        state: None,
        pull_request_ids: Vec::new(),
        task_disabled: task.pr_auto_complete_disabled,
        workspace_enabled: settings.move_on_merge,
        target_status: settings.move_on_merge.then_some(target),
    };
    let ids = |keep: fn(PrState) -> bool| -> Vec<PullRequestId> {
        prs.iter().filter(|p| keep(p.state)).map(|p| p.id).collect()
    };
    let open = ids(|s| matches!(s, PrState::Open | PrState::Draft));
    let closed = ids(|s| s == PrState::Closed);
    decision.state = if prs.is_empty() {
        None
    } else if !task.status.is_open() {
        Terminal
    } else if !settings.move_on_merge {
        WorkspaceDisabled
    } else if task.pr_auto_complete_disabled {
        TaskDisabled
    } else if task.status == target {
        AtTarget
    } else if !open.is_empty() {
        decision.pull_request_ids = open;
        Waiting
    } else if !closed.is_empty() {
        decision.pull_request_ids = closed;
        NotMerged
    } else {
        decision.pull_request_ids = prs.iter().map(|p| p.id).collect();
        AllMerged
    };
    decision
}

/// Where a merge moves `task` (decision 6): `done` when its latest run was
/// a workflow's, `needs_you` when an agent's system workflow did the work
/// or nothing ran yet (a person confirms).
async fn target_of(tx: &mut Tx, task: &Task) -> Result<TaskStatus> {
    let latest = tx.runs_of_task(task.id).await?.pop();
    Ok(match latest {
        Some(run) if tx.system_agent_of(run.workflow_id).await?.is_none() => TaskStatus::Done,
        _ => TaskStatus::NeedsYou,
    })
}

/// One PR on the task page.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PullRequestView {
    /// The mirrored PR.
    #[serde(flatten)]
    pub pull_request: PullRequest,
    /// A snapshot exists for the current head.
    pub snapshot_available: bool,
    /// How it got on the task.
    pub link_source: LinkSource,
}

/// `GET /tasks/{id}/pull_requests`: the task's PRs and the merge rule.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TaskPullRequests {
    /// The PRs, oldest link first.
    pub pull_requests: Vec<PullRequestView>,
    /// The merge rule's decision.
    pub auto_complete: AutoComplete,
}

/// The task's PRs and the merge rule's decision, written nowhere.
async fn view(tx: &mut Tx, task: &Task) -> Result<TaskPullRequests> {
    let workspace = tx.get_workspace(task.workspace_id).await?;
    let key = task.key(workspace.issue_prefix.clone())?;
    let links = tx.pull_requests_of_task(task.id).await?;
    let prs: Vec<PullRequest> = links.iter().map(|(_, pr)| pr.clone()).collect();
    let target = target_of(tx, task).await?;
    let auto_complete = decide(task, &prs, workspace.git_settings, target);
    let pull_requests = links
        .into_iter()
        .map(|(link, pr)| PullRequestView {
            snapshot_available: pr.snapshot_available(),
            link_source: link_source(&link, &pr, &key),
            pull_request: pr,
        })
        .collect();
    Ok(TaskPullRequests {
        pull_requests,
        auto_complete,
    })
}

/// `#12, #15` for a status activity.
fn numbers(prs: &[PullRequest]) -> String {
    prs.iter()
        .map(|p| format!("#{}", p.number))
        .collect::<Vec<_>>()
        .join(", ")
}

/// Runs the merge rule for `task` after a PR event, moving it when every
/// linked PR has merged, with a status activity `{source: pr_automation,
/// pull_requests: "#12, #15"}`. Safe for any task: every guard is in
/// [`decide`], and the store writes only while the task is still open.
async fn maybe_auto_complete(tx: &mut Tx, task: TaskId) -> Result<()> {
    let task = tx.get_task(task).await?;
    let workspace = tx.get_workspace(task.workspace_id).await?;
    let prs: Vec<PullRequest> = tx
        .pull_requests_of_task(task.id)
        .await?
        .into_iter()
        .map(|(_, pr)| pr)
        .collect();
    let target = target_of(tx, &task).await?;
    let decision = decide(&task, &prs, workspace.git_settings, target);
    if decision.state != AutoCompleteState::AllMerged {
        return Ok(());
    }
    if let Some(from) = tx.set_task_status_by_merge(task.id, target).await? {
        let moved = json!({
            "from": from,
            "to": target,
            "source": "pr_automation",
            "pull_requests": numbers(&prs),
        });
        tx.append_activity(task.id, ActivityKind::Status, None, moved)
            .await?;
    }
    Ok(())
}

/// The task's `pull_request` activity `{action, number, url, repo, source}`.
async fn log_link(
    tx: &mut Tx,
    task: TaskId,
    pr: &PullRequest,
    action: &str,
    source: LinkSource,
    by: Option<MemberId>,
) -> Result<()> {
    let body = json!({
        "action": action,
        "number": pr.number,
        "url": pr.html_url,
        "repo": pr.repo(),
        "source": source,
    });
    tx.append_activity(task, ActivityKind::PullRequest, by, body)
        .await?;
    Ok(())
}

/// Makes the PR's automatic links match the keys `event` claims, on this
/// workspace's tasks only. A claimed task links unless a person removed
/// this PR from it before. An automatic link the PR no longer claims is
/// dropped while the PR is open or draft; after a merge or close it stays
/// (a title edit must not take delivered work off a task). Manual links are
/// never touched. Returns the tasks whose link set changed.
async fn reconcile_auto_links(
    tx: &mut Tx,
    workspace: &Workspace,
    pr: &PullRequest,
    event: &PrEvent,
) -> Result<Vec<TaskId>> {
    let mut touched = Vec::new();
    let mut claimed = Vec::new();
    let branch = event.branch.as_deref().unwrap_or_default();
    for key in claimed_keys(&event.title, &event.body, branch) {
        if !key.prefix.eq_ignore_ascii_case(&workspace.issue_prefix) {
            continue;
        }
        let task = match tx.task_by_key(&key).await {
            Ok(task) if task.workspace_id == workspace.id => task,
            Ok(_) => continue,
            Err(e) => match Error::from(e) {
                Error::NotFound(_) => continue,
                e => return Err(e),
            },
        };
        if tx.is_excluded(task.id, pr.id).await? {
            continue;
        }
        claimed.push(task.id);
        if PullRequestRepo::link(tx, task.id, pr.id, None).await? {
            let link = TaskPullRequest {
                task_id: task.id,
                pull_request_id: pr.id,
                linked_by: None,
                linked_at: chrono::Utc::now(),
            };
            let source = link_source(&link, pr, &key);
            log_link(tx, task.id, pr, "linked", source, None).await?;
            touched.push(task.id);
        }
    }
    if matches!(pr.state, PrState::Merged | PrState::Closed) {
        return Ok(touched);
    }
    for task in tx.auto_linked_tasks(pr.id).await? {
        if claimed.contains(&task) {
            continue;
        }
        if PullRequestRepo::unlink(tx, task, pr.id).await? {
            log_link(tx, task, pr, "unlinked", LinkSource::Auto, None).await?;
            touched.push(task);
        }
    }
    Ok(touched)
}

/// Applies `event` to `workspace` in the caller's transaction: the row is
/// always upserted (so turning GitHub back on needs no backfill), while
/// links and status moves need `github_enabled`. A stale delivery (the row
/// is newer) does nothing and returns `None`. A merge is a PR event for
/// every linked task, manual links included, once: when the stored state
/// turns merged.
pub(crate) async fn mirror(
    tx: &mut Tx,
    workspace: &Workspace,
    event: &PrEvent,
) -> Result<Option<PullRequest>> {
    let before = tx
        .pull_request_by_url(workspace.id, &event.html_url)
        .await?
        .map(|pr| pr.state);
    let Some(pr) = tx
        .upsert_pull_request(&event.pull_request(workspace.id))
        .await?
    else {
        return Ok(None);
    };
    let settings = workspace.git_settings;
    if !settings.github_enabled {
        return Ok(Some(pr));
    }
    let mut touched = match settings.auto_link {
        true => reconcile_auto_links(tx, workspace, &pr, event).await?,
        false => Vec::new(),
    };
    if pr.state == PrState::Merged && before != Some(PrState::Merged) {
        for link in tx.tasks_of_pull_request(pr.id).await? {
            let task = tx.get_task(link.task_id).await?;
            let key = task.key(workspace.issue_prefix.clone())?;
            let source = link_source(&link, &pr, &key);
            log_link(tx, task.id, &pr, "merged", source, None).await?;
            if !touched.contains(&task.id) {
                touched.push(task.id);
            }
        }
    }
    for task in touched {
        maybe_auto_complete(tx, task).await?;
    }
    Ok(Some(pr))
}

/// The head commit a GitHub check or status delivery is about.
fn delivery_head(event: &str, payload: &Value) -> Option<String> {
    let sha = match event {
        "status" => &payload["sha"],
        "check_suite" => &payload["check_suite"]["head_sha"],
        "check_run" => &payload["check_run"]["check_suite"]["head_sha"],
        _ => &Value::Null,
    };
    text(sha)
}

/// A GitLab pipeline as one check: `success` and `skipped` passed,
/// `failed` and `canceled` failed, anything else still running.
fn pipeline_snapshot(sha: String, status: &str) -> PrSnapshot {
    let (rollup, passed, failed, pending) = match status {
        "success" | "skipped" => ("SUCCESS", 1, 0, 0),
        "failed" | "canceled" => ("FAILURE", 0, 1, 0),
        _ => ("PENDING", 0, 0, 1),
    };
    PrSnapshot {
        head_sha: sha,
        mergeable: None,
        merge_state_status: None,
        checks_rollup_state: Some(rollup.into()),
        checks_total: 1,
        checks_passed: passed,
        checks_failed: failed,
        checks_pending: pending,
    }
}

impl Engine {
    /// A verified GitHub App delivery (`X-GitHub-Event` = `event`): an
    /// `installation` removal or suspension forgets its connections; a
    /// `pull_request` is mirrored into every workspace connected to the
    /// installation and its snapshot refreshed; `check_suite`, `check_run`
    /// and `status` refresh the snapshot of the PRs at their head. Other
    /// events are ignored.
    pub async fn github_delivery(&self, event: &str, payload: &Value) -> Result<()> {
        match event {
            "installation" => {
                if matches!(payload["action"].as_str(), Some("deleted" | "suspend"))
                    && let Some(installation) = payload["installation"]["id"].as_i64()
                {
                    self.forget_github_installation(installation).await?;
                }
            }
            "pull_request" => {
                let Some(event) = PrEvent::from_github(payload) else {
                    return Ok(());
                };
                let Some(installation) = event.installation_id else {
                    return Ok(());
                };
                let mut tx = self.store.begin().await?;
                let mut mirrored = Vec::new();
                for connection in tx.connections_of_installation(installation).await? {
                    let workspace = tx.get_workspace(connection.workspace_id).await?;
                    if let Some(pr) = mirror(&mut tx, &workspace, &event).await? {
                        mirrored.push(pr.id);
                    }
                }
                tx.commit().await?;
                self.refresh_snapshots_later(mirrored);
            }
            "check_suite" | "check_run" | "status" => {
                let (Some(installation), Some(sha)) = (
                    payload["installation"]["id"].as_i64(),
                    delivery_head(event, payload),
                ) else {
                    return Ok(());
                };
                let repo = &payload["repository"];
                let (Some(owner), Some(name)) =
                    (text(&repo["owner"]["login"]), text(&repo["name"]))
                else {
                    return Ok(());
                };
                let mut tx = self.store.read().await?;
                let mut at_head = Vec::new();
                for connection in tx.connections_of_installation(installation).await? {
                    let prs = tx
                        .pull_requests_by_head(connection.workspace_id, &owner, &name, &sha)
                        .await?;
                    at_head.extend(prs.into_iter().map(|pr| pr.id));
                }
                self.refresh_snapshots_later(at_head);
            }
            _ => {}
        }
        Ok(())
    }

    /// A verified GitLab hook on `connection` (`X-Gitlab-Event` = `event`):
    /// a Merge Request Hook is mirrored into the connection's workspace; a
    /// Pipeline Hook becomes the one-check snapshot of the MRs at its
    /// commit. Other events are ignored.
    pub async fn gitlab_delivery(
        &self,
        connection: &VcsConnection,
        event: &str,
        payload: &Value,
    ) -> Result<()> {
        match event {
            "Merge Request Hook" => {
                let Some(event) = PrEvent::from_gitlab(payload) else {
                    return Ok(());
                };
                let mut tx = self.store.begin().await?;
                let workspace = tx.get_workspace(connection.workspace_id).await?;
                mirror(&mut tx, &workspace, &event).await?;
                tx.commit().await?;
            }
            "Pipeline Hook" => {
                let pipeline = &payload["object_attributes"];
                let (Some(sha), Some(status), Some((owner, name))) = (
                    text(&pipeline["sha"]),
                    pipeline["status"].as_str(),
                    payload["project"]["path_with_namespace"]
                        .as_str()
                        .and_then(split_namespace),
                ) else {
                    return Ok(());
                };
                let mut tx = self.store.begin().await?;
                let prs = tx
                    .pull_requests_by_head(connection.workspace_id, &owner, &name, &sha)
                    .await?;
                let snapshot = pipeline_snapshot(sha, status);
                for pr in prs {
                    tx.set_snapshot(pr.id, &snapshot).await?;
                }
                tx.commit().await?;
            }
            _ => {}
        }
        Ok(())
    }

    /// Fetches the GitHub PR's mergeability and checks and stores them;
    /// false when the PR isn't GitHub's, its installation has no connection
    /// left, or its head moved while fetching (the snapshot is dropped).
    pub async fn refresh_snapshot(&self, id: PullRequestId) -> Result<bool> {
        let (pr, connection) = {
            let mut tx = self.store.read().await?;
            let pr = tx.get_pull_request(id).await?;
            if pr.provider != VcsProvider::Github {
                return Ok(false);
            }
            let connection = tx
                .connections(pr.workspace_id)
                .await?
                .into_iter()
                .find(|c| {
                    c.provider == VcsProvider::Github && c.installation_id == pr.installation_id
                });
            match connection {
                Some(connection) => (pr, connection),
                None => return Ok(false),
            }
        };
        let credentials = self.vcs.credentials(&connection, false).await?;
        let snapshot = crate::vcs::pr_snapshot(
            &credentials.api_url,
            &credentials.token,
            &pr.repo_owner,
            &pr.repo_name,
            pr.number,
        )
        .await?;
        let mut tx = self.store.begin().await?;
        let stored = tx.set_snapshot(pr.id, &snapshot).await?;
        tx.commit().await?;
        Ok(stored)
    }

    /// Refreshes the snapshots of `ids` in the background, after the
    /// delivery's transaction committed.
    fn refresh_snapshots_later(&self, ids: Vec<PullRequestId>) {
        if ids.is_empty() {
            return;
        }
        let engine = self.clone();
        tokio::spawn(async move {
            for id in ids {
                if let Err(e) = engine.refresh_snapshot(id).await {
                    eprintln!("sun: pull request {id} snapshot: {e}");
                }
            }
        });
    }

    /// `GET /tasks/{id}/pull_requests`: the task's PRs and the merge rule.
    pub async fn task_pull_requests(&self, task: TaskId) -> Result<TaskPullRequests> {
        let mut tx = self.store.read().await?;
        let task = tx.get_task(task).await?;
        view(&mut tx, &task).await
    }

    /// Links a PR the workspace already mirrors to `task` by hand: by
    /// `url` (a pasted PR page) or by `pull_request_id` (undo of a removal).
    /// `NotFound` until the PR's delivery arrived. The link becomes manual
    /// even when it was automatic, a removal is forgotten, and the merge
    /// rule runs (linking is a PR event).
    pub async fn link_pull_request(
        &self,
        task: TaskId,
        by: MemberId,
        url: Option<String>,
        pull_request_id: Option<PullRequestId>,
    ) -> Result<TaskPullRequests> {
        let mut tx = self.store.begin().await?;
        let task = tx.get_task(task).await?;
        let pr = match (pull_request_id, url) {
            (Some(id), _) => match tx.get_pull_request(id).await {
                Ok(pr) if pr.workspace_id == task.workspace_id => Some(pr),
                Ok(_) => None,
                Err(e) => match Error::from(e) {
                    Error::NotFound(_) => None,
                    e => return Err(e),
                },
            },
            (None, Some(url)) => match normalize_pr_url(&url) {
                Some(url) => tx.pull_request_by_url(task.workspace_id, &url).await?,
                None => None,
            },
            (None, None) => {
                return Err(Error::InvalidInput("Paste a pull request URL.".into()));
            }
        };
        let Some(pr) = pr else {
            return Err(Error::NotFound(
                "pull request not found; it appears here once its repository is connected \
                 and the PR has been synced"
                    .into(),
            ));
        };
        // `link` keeps an existing (automatic) row, so it is replaced.
        let was_manual = tx
            .tasks_of_pull_request(pr.id)
            .await?
            .iter()
            .any(|l| l.task_id == task.id && l.linked_by.is_some());
        PullRequestRepo::unlink(&mut tx, task.id, pr.id).await?;
        PullRequestRepo::link(&mut tx, task.id, pr.id, Some(by)).await?;
        tx.unexclude(task.id, pr.id).await?;
        if !was_manual {
            log_link(
                &mut tx,
                task.id,
                &pr,
                "linked",
                LinkSource::Manual,
                Some(by),
            )
            .await?;
            maybe_auto_complete(&mut tx, task.id).await?;
        }
        let task = tx.get_task(task.id).await?;
        let out = view(&mut tx, &task).await?;
        tx.commit().await?;
        Ok(out)
    }

    /// Removes a PR from `task` and remembers it, so the next delivery for
    /// that PR doesn't link it again from its title or branch. The merge
    /// rule runs (removing the last unmerged PR can leave only merged ones).
    pub async fn unlink_pull_request(
        &self,
        task: TaskId,
        pull_request: PullRequestId,
        by: MemberId,
    ) -> Result<TaskPullRequests> {
        let mut tx = self.store.begin().await?;
        let task = tx.get_task(task).await?;
        let pr = tx.get_pull_request(pull_request).await?;
        if pr.workspace_id != task.workspace_id {
            return Err(Error::NotFound(format!("pull request {pull_request}")));
        }
        let removed = PullRequestRepo::unlink(&mut tx, task.id, pr.id).await?;
        tx.exclude(task.id, pr.id, Some(by)).await?;
        if removed {
            log_link(
                &mut tx,
                task.id,
                &pr,
                "unlinked",
                LinkSource::Manual,
                Some(by),
            )
            .await?;
            maybe_auto_complete(&mut tx, task.id).await?;
        }
        let task = tx.get_task(task.id).await?;
        let out = view(&mut tx, &task).await?;
        tx.commit().await?;
        Ok(out)
    }

    /// Turns "move when PRs merge" off (or back on) for one task. Turning
    /// it back on moves nothing by itself; the next PR event does.
    pub async fn set_pr_auto_complete(
        &self,
        task: TaskId,
        disabled: bool,
        by: MemberId,
    ) -> Result<TaskPullRequests> {
        let mut tx = self.store.begin().await?;
        let before = tx.get_task(task).await?;
        if before.pr_auto_complete_disabled != disabled {
            tx.set_pr_auto_complete(task, disabled).await?;
            let body = json!({ "action": "auto_complete", "disabled": disabled });
            tx.append_activity(task, ActivityKind::PullRequest, Some(by), body)
                .await?;
        }
        let task = tx.get_task(task).await?;
        let out = view(&mut tx, &task).await?;
        tx.commit().await?;
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use sun_core::model::{Agent, Budget, Run, Workflow};
    use sun_core::{AgentId, RunId, RunStatus, WorkflowId};
    use sun_store::{MemberRepo, Store, WorkflowRepo};

    use super::*;

    fn key(text: &str) -> TaskKey {
        text.parse().unwrap()
    }

    #[test]
    fn claimed_keys_come_from_title_branch_and_closing_keywords() {
        let keys = claimed_keys(
            "SUN-12 fix login (see sun-12, ENG-3)",
            "Mentions SUN-40 but fixes SUN-41.\nCloses: eng-5\nResolved ENG-6 and fix login SUN-99",
            "sun/sun-12-fix-login",
        );
        let want: Vec<TaskKey> = ["SUN-12", "ENG-3", "SUN-41", "ENG-5", "ENG-6"]
            .iter()
            .map(|k| key(k))
            .collect();
        assert_eq!(keys, want);
        // No key in a version or a zero number.
        assert!(claimed_keys("v1.2-3 SUN-0 1-2", "", "").is_empty());
        assert_eq!(claimed_keys("", "", "sun/SUN-7-x"), vec![key("SUN-7")]);
    }

    #[test]
    fn pr_urls_are_normalised() {
        let want = Some("https://github.com/hryer/vps-infra/pull/12".to_owned());
        assert_eq!(
            normalize_pr_url("https://github.com/hryer/vps-infra/pull/12"),
            want
        );
        assert_eq!(
            normalize_pr_url(" HTTPS://GitHub.com/hryer/vps-infra/pull/12/files?tab=1#x "),
            want
        );
        assert_eq!(
            normalize_pr_url("github.com/hryer/vps-infra/pull/12/"),
            want
        );
        assert_eq!(
            normalize_pr_url("https://gitlab.com/g/sub/repo/-/merge_requests/3"),
            Some("https://gitlab.com/g/sub/repo/-/merge_requests/3".into())
        );
        assert_eq!(normalize_pr_url("https://github.com/hryer/vps-infra"), None);
        assert_eq!(
            normalize_pr_url("https://github.com/hryer/vps-infra/issues/12"),
            None
        );
        assert_eq!(normalize_pr_url(""), None);
    }

    fn github_payload(number: i64, title: &str, branch: &str, state: &str, merged: bool) -> Value {
        json!({
            "action": "opened",
            "installation": { "id": 77 },
            "repository": { "name": "vps-infra", "owner": { "login": "hryer" } },
            "pull_request": {
                "number": number, "title": title, "body": "",
                "state": state, "draft": false, "merged": merged,
                "html_url": format!("https://github.com/hryer/vps-infra/pull/{number}"),
                "head": { "ref": branch, "sha": "abc" },
                "user": { "login": "ann", "avatar_url": "https://a/ann.png" },
                "created_at": "2026-10-11T10:00:00Z", "updated_at": "2026-10-11T10:00:00Z",
                "merged_at": if merged { json!("2026-10-11T11:00:00Z") } else { Value::Null },
                "additions": 3, "deletions": 1, "changed_files": 2
            }
        })
    }

    #[test]
    fn github_and_gitlab_payloads_become_events() {
        let event =
            PrEvent::from_github(&github_payload(12, "SUN-1 x", "sun/SUN-1-x", "open", false))
                .unwrap();
        assert_eq!(event.state, PrState::Open);
        assert_eq!(event.installation_id, Some(77));
        assert_eq!(
            (event.repo_owner.as_str(), event.repo_name.as_str()),
            ("hryer", "vps-infra")
        );
        assert_eq!(event.additions, 3);
        let mut draft = github_payload(12, "x", "b", "open", false);
        draft["pull_request"]["draft"] = json!(true);
        assert_eq!(PrEvent::from_github(&draft).unwrap().state, PrState::Draft);
        assert_eq!(
            PrEvent::from_github(&github_payload(12, "x", "b", "closed", false))
                .unwrap()
                .state,
            PrState::Closed
        );
        // Merged wins over closed, closed over draft.
        let mut merged = github_payload(12, "x", "b", "closed", true);
        merged["pull_request"]["draft"] = json!(true);
        assert_eq!(
            PrEvent::from_github(&merged).unwrap().state,
            PrState::Merged
        );
        assert_eq!(PrEvent::from_github(&json!({ "zen": "x" })), None);

        let gitlab = json!({
            "object_kind": "merge_request",
            "user": { "username": "ann", "avatar_url": "https://a/ann.png" },
            "project": { "path_with_namespace": "group/sub/repo" },
            "object_attributes": {
                "iid": 3, "title": "Draft: SUN-2 thing", "description": "Closes SUN-3",
                "state": "opened", "source_branch": "sun/SUN-2-thing",
                "url": "https://gitlab.com/group/sub/repo/-/merge_requests/3",
                "work_in_progress": false, "draft": false,
                "created_at": "2026-10-11 10:00:00 UTC", "updated_at": "2026-10-11 10:05:00 UTC",
                "last_commit": { "id": "def" }
            }
        });
        let event = PrEvent::from_gitlab(&gitlab).unwrap();
        assert_eq!(event.provider, VcsProvider::Gitlab);
        assert_eq!(
            (event.repo_owner.as_str(), event.repo_name.as_str()),
            ("group/sub", "repo")
        );
        assert_eq!(event.number, 3);
        assert_eq!(event.state, PrState::Draft);
        assert_eq!(event.head_sha, "def");
        assert_eq!(event.updated_at.to_rfc3339(), "2026-10-11T10:05:00+00:00");
        assert_eq!(
            claimed_keys(&event.title, &event.body, event.branch.as_deref().unwrap()),
            vec![key("SUN-2"), key("SUN-3")]
        );
        let mut locked = gitlab.clone();
        locked["object_attributes"]["state"] = json!("locked");
        locked["object_attributes"]["title"] = json!("SUN-2 thing");
        assert_eq!(PrEvent::from_gitlab(&locked).unwrap().state, PrState::Open);
    }

    fn pr(id: PullRequestId, number: i64, state: PrState) -> PullRequest {
        let mut pr = PrEvent::from_github(&github_payload(number, "x", "b", "open", false))
            .unwrap()
            .pull_request(WorkspaceId::MAIN);
        pr.id = id;
        pr.state = state;
        pr
    }

    #[test]
    fn decision_matrix() {
        use AutoCompleteState::*;
        let (a, b) = (PullRequestId::generate(), PullRequestId::generate());
        let task = Task {
            status: TaskStatus::InProgress,
            ..Task::new(WorkspaceId::MAIN, "t")
        };
        let on = GitSettings::default();
        let decision = |task: &Task, prs: &[PullRequest], settings, target| {
            let d = decide(task, prs, settings, target);
            (d.state, d.pull_request_ids.clone(), d.target_status)
        };
        let done = TaskStatus::Done;
        assert_eq!(decision(&task, &[], on, done), (None, vec![], Some(done)));
        let merged = [pr(a, 1, PrState::Merged), pr(b, 2, PrState::Merged)];
        assert_eq!(
            decision(&task, &merged, on, done),
            (AllMerged, vec![a, b], Some(done))
        );
        let closed_task = Task {
            status: TaskStatus::Done,
            ..task.clone()
        };
        assert_eq!(decision(&closed_task, &merged, on, done).0, Terminal);
        let off = GitSettings {
            move_on_merge: false,
            ..on
        };
        assert_eq!(
            decision(&task, &merged, off, done),
            (WorkspaceDisabled, vec![], Option::None)
        );
        let disabled = Task {
            pr_auto_complete_disabled: true,
            ..task.clone()
        };
        let d = decide(&disabled, &merged, on, done);
        assert_eq!(
            (d.state, d.task_disabled, d.workspace_enabled),
            (TaskDisabled, true, true)
        );
        // Workspace off wins over task off.
        assert_eq!(decision(&disabled, &merged, off, done).0, WorkspaceDisabled);
        let needs_you = Task {
            status: TaskStatus::NeedsYou,
            ..task.clone()
        };
        assert_eq!(
            decision(&needs_you, &merged, on, TaskStatus::NeedsYou).0,
            AtTarget
        );
        assert_eq!(decision(&needs_you, &merged, on, done).0, AllMerged);
        let waiting = [pr(a, 1, PrState::Merged), pr(b, 2, PrState::Draft)];
        assert_eq!(
            decision(&task, &waiting, on, done),
            (Waiting, vec![b], Some(done))
        );
        let not_merged = [pr(a, 1, PrState::Closed), pr(b, 2, PrState::Merged)];
        assert_eq!(
            decision(&task, &not_merged, on, done),
            (NotMerged, vec![a], Some(done))
        );
    }

    #[test]
    fn link_source_reads_title_then_branch() {
        let mut pr = pr(PullRequestId::generate(), 1, PrState::Open);
        pr.title = "SUN-12 fix".into();
        pr.branch = Some("sun/SUN-13-x".into());
        let auto = TaskPullRequest {
            task_id: TaskId::generate(),
            pull_request_id: pr.id,
            linked_by: Option::None,
            linked_at: chrono::Utc::now(),
        };
        let manual = TaskPullRequest {
            linked_by: Some(MemberId::generate()),
            ..auto.clone()
        };
        assert_eq!(link_source(&auto, &pr, &key("SUN-12")), LinkSource::Title);
        assert_eq!(link_source(&auto, &pr, &key("SUN-13")), LinkSource::Branch);
        assert_eq!(link_source(&auto, &pr, &key("SUN-14")), LinkSource::Auto);
        assert_eq!(
            link_source(&manual, &pr, &key("SUN-12")),
            LinkSource::Manual
        );
    }

    #[test]
    fn views_serialise_in_snake_case() {
        let view = TaskPullRequests {
            pull_requests: vec![PullRequestView {
                pull_request: pr(PullRequestId::generate(), 1, PrState::Open),
                snapshot_available: false,
                link_source: LinkSource::Branch,
            }],
            auto_complete: AutoComplete {
                state: AutoCompleteState::WorkspaceDisabled,
                pull_request_ids: vec![],
                task_disabled: false,
                workspace_enabled: false,
                target_status: Option::None,
            },
        };
        let json = serde_json::to_value(&view).unwrap();
        assert_eq!(json["pull_requests"][0]["link_source"], "branch");
        assert_eq!(json["pull_requests"][0]["number"], 1);
        assert_eq!(json["pull_requests"][0]["snapshot_available"], false);
        assert_eq!(json["auto_complete"]["state"], "workspace_disabled");
        assert_eq!(json["auto_complete"]["target_status"], Value::Null);
    }

    /// Main (prefix `MAIN`) with a workflow task, an agent task and a member.
    struct Seed {
        store: Store,
        workspace: Workspace,
        workflow_task: Task,
        agent_task: Task,
        member: MemberId,
    }

    async fn seed() -> (tempfile::TempDir, Seed) {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).await.unwrap();
        let mut tx = store.begin().await.unwrap();
        let workspace = tx.get_workspace(WorkspaceId::MAIN).await.unwrap();
        let workflow = Workflow {
            id: WorkflowId::generate(),
            workspace_id: workspace.id,
            name: "ship".into(),
        };
        tx.insert_workflow(&workflow).await.unwrap();
        tx.save_workflow_version(workflow.id, "name: ship\nsteps: []\n", "h")
            .await
            .unwrap();
        let agent = Agent {
            id: AgentId::generate(),
            workspace_id: WorkspaceId::MAIN,
            name: "dev".into(),
            description: String::new(),
            instructions: String::new(),
            runtime_chain: vec!["fake".into()],
            skill_ids: vec![],
            created_at: chrono::Utc::now(),
            archived_at: Option::None,
        };
        tx.insert_agent(&agent).await.unwrap();
        let system = Workflow {
            id: WorkflowId::generate(),
            workspace_id: workspace.id,
            name: "dev".into(),
        };
        tx.insert_system_workflow(&system, agent.id).await.unwrap();
        tx.save_workflow_version(system.id, "name: dev\nsteps: []\n", "h")
            .await
            .unwrap();
        let mut workflow_task = Task {
            workflow_id: Some(workflow.id),
            status: TaskStatus::InProgress,
            ..Task::new(workspace.id, "fix login")
        };
        workflow_task.number = tx.insert_task(&workflow_task).await.unwrap();
        let mut agent_task = Task {
            workflow_id: Some(system.id),
            assignee_agent_id: Some(agent.id),
            status: TaskStatus::InProgress,
            ..Task::new(workspace.id, "add search")
        };
        agent_task.number = tx.insert_task(&agent_task).await.unwrap();
        for task in [&workflow_task, &agent_task] {
            let run = Run {
                id: RunId::generate(),
                task_id: task.id,
                workflow_id: task.workflow_id.unwrap(),
                workflow_version: 1,
                status: RunStatus::Succeeded,
                end_reason: Option::None,
                budget: Budget::default(),
                repo_ids: vec![],
            };
            tx.insert_run(&run).await.unwrap();
        }
        let member = tx
            .upsert_member("local", "ann@sun.test", "ann", Some("ann@sun.test"), false)
            .await
            .unwrap()
            .id;
        tx.commit().await.unwrap();
        (
            dir,
            Seed {
                store,
                workspace,
                workflow_task,
                agent_task,
                member,
            },
        )
    }

    impl Seed {
        /// The merge rule's decision for `task`, as the task page sees it.
        async fn decision(&self, task: TaskId) -> AutoComplete {
            let mut tx = self.store.read().await.unwrap();
            let task = tx.get_task(task).await.unwrap();
            view(&mut tx, &task).await.unwrap().auto_complete
        }

        /// Stores the workspace's "move tasks when their PRs merge" switch.
        async fn move_on_merge(&self, on: bool) {
            let mut tx = self.store.begin().await.unwrap();
            let mut workspace = tx.get_workspace(self.workspace.id).await.unwrap();
            workspace.git_settings.move_on_merge = on;
            tx.update_workspace(&workspace).await.unwrap();
            tx.commit().await.unwrap();
        }
    }

    impl Seed {
        fn key(&self, task: &Task) -> String {
            format!("{}-{}", self.workspace.issue_prefix, task.number.unwrap())
        }

        async fn deliver(&self, payload: &Value) -> Option<PullRequest> {
            let event = PrEvent::from_github(payload).unwrap();
            let mut tx = self.store.begin().await.unwrap();
            let pr = mirror(&mut tx, &self.workspace, &event).await.unwrap();
            tx.commit().await.unwrap();
            pr
        }

        async fn linked(&self, task: &Task) -> Vec<(i64, LinkSource)> {
            let mut tx = self.store.read().await.unwrap();
            let task = tx.get_task(task.id).await.unwrap();
            view(&mut tx, &task)
                .await
                .unwrap()
                .pull_requests
                .iter()
                .map(|p| (p.pull_request.number, p.link_source))
                .collect()
        }

        async fn status(&self, task: &Task) -> TaskStatus {
            self.store
                .read()
                .await
                .unwrap()
                .get_task(task.id)
                .await
                .unwrap()
                .status
        }

        async fn activity(&self, task: &Task) -> Vec<Value> {
            self.store
                .read()
                .await
                .unwrap()
                .activity_of(task.id, 0)
                .await
                .unwrap()
                .into_iter()
                .map(|a| json!({ "kind": a.kind, "body": a.body }))
                .collect()
        }
    }

    #[tokio::test]
    async fn reconcile_links_by_claim_and_keeps_manual_and_terminal_links() {
        let (_dir, seed) = seed().await;
        let (wt, at) = (&seed.workflow_task, &seed.agent_task);
        let (wk, ak) = (seed.key(wt), seed.key(at));
        // Title claims the workflow task, branch the agent task.
        let payload = github_payload(
            1,
            &format!("{wk} fix"),
            &format!("sun/{ak}-x"),
            "open",
            false,
        );
        let pr = seed.deliver(&payload).await.unwrap();
        assert_eq!(seed.linked(wt).await, vec![(1, LinkSource::Title)]);
        assert_eq!(seed.linked(at).await, vec![(1, LinkSource::Branch)]);
        // A key of another workspace's prefix, or a missing number, links nothing.
        let other = github_payload(
            2,
            &format!("ENG-1 {}-999", seed.workspace.issue_prefix),
            "b",
            "open",
            false,
        );
        seed.deliver(&other).await.unwrap();
        assert_eq!(seed.linked(wt).await.len(), 1);
        // An edit drops the claim on the agent task while open; the stale
        // delivery (older updated_at) is a no-op.
        let mut edited = github_payload(1, &format!("{wk} fix"), "b", "open", false);
        edited["pull_request"]["updated_at"] = json!("2026-10-11T12:00:00Z");
        assert!(seed.deliver(&edited).await.is_some());
        assert_eq!(seed.linked(at).await, vec![]);
        let mut stale = github_payload(1, "nothing", "b", "open", false);
        stale["pull_request"]["updated_at"] = json!("2026-10-11T09:00:00Z");
        assert_eq!(seed.deliver(&stale).await, Option::None);
        assert_eq!(seed.linked(wt).await, vec![(1, LinkSource::Title)]);
        // A manual link is never touched by a delivery; a removal sticks.
        {
            let mut tx = seed.store.begin().await.unwrap();
            PullRequestRepo::link(&mut tx, at.id, pr.id, Some(seed.member))
                .await
                .unwrap();
            tx.commit().await.unwrap();
        }
        let mut again = edited.clone();
        again["pull_request"]["updated_at"] = json!("2026-10-11T13:00:00Z");
        seed.deliver(&again).await.unwrap();
        assert_eq!(seed.linked(at).await, vec![(1, LinkSource::Manual)]);
        {
            let mut tx = seed.store.begin().await.unwrap();
            PullRequestRepo::unlink(&mut tx, wt.id, pr.id)
                .await
                .unwrap();
            tx.exclude(wt.id, pr.id, Option::None).await.unwrap();
            tx.commit().await.unwrap();
        }
        let mut third = edited.clone();
        third["pull_request"]["updated_at"] = json!("2026-10-11T14:00:00Z");
        seed.deliver(&third).await.unwrap();
        assert_eq!(seed.linked(wt).await, vec![]);
        // Merged: a title edit no longer drops an automatic link.
        let payload = github_payload(3, &format!("{wk} more"), "b", "open", false);
        seed.deliver(&payload).await.unwrap();
        let mut merged = github_payload(3, "renamed", "b", "closed", true);
        merged["pull_request"]["updated_at"] = json!("2026-10-11T15:00:00Z");
        seed.deliver(&merged).await.unwrap();
        assert_eq!(seed.linked(wt).await, vec![(3, LinkSource::Auto)]);
    }

    #[tokio::test]
    async fn merge_moves_workflow_tasks_to_done_and_agent_tasks_to_needs_you() {
        let (_dir, seed) = seed().await;
        let (wt, at) = (&seed.workflow_task, &seed.agent_task);
        let (wk, ak) = (seed.key(wt), seed.key(at));
        let open = |number: i64, title: String| github_payload(number, &title, "b", "open", false);
        let merge = |number: i64, title: String, at: &str| {
            let mut p = github_payload(number, &title, "b", "closed", true);
            p["pull_request"]["updated_at"] = json!(at);
            p
        };
        seed.deliver(&open(1, format!("{wk} a"))).await;
        seed.deliver(&open(2, format!("{wk} b"))).await;
        seed.deliver(&open(3, format!("{ak} c"))).await;
        // With the workspace switch off a merge moves nothing.
        seed.move_on_merge(false).await;
        seed.deliver(&merge(3, format!("{ak} c"), "2026-10-11T12:00:00Z"))
            .await;
        assert_eq!(seed.status(at).await, TaskStatus::InProgress);
        let off = seed.decision(at.id).await;
        assert_eq!(off.state, AutoCompleteState::WorkspaceDisabled);
        assert_eq!(
            (off.workspace_enabled, off.target_status),
            (false, Option::None)
        );
        // Turning the switch back on moves nothing by itself, and a later
        // delivery of the merged PR (a label) is not a merge event.
        seed.move_on_merge(true).await;
        seed.deliver(&merge(3, format!("{ak} c"), "2026-10-11T12:30:00Z"))
            .await;
        assert_eq!(seed.status(at).await, TaskStatus::InProgress);
        let on = seed.decision(at.id).await;
        assert_eq!(on.state, AutoCompleteState::AllMerged);
        assert_eq!(on.target_status, Some(TaskStatus::NeedsYou));
        // A new PR for the agent task, merged: every linked PR merged → needs_you.
        seed.deliver(&open(5, format!("{ak} e"))).await;
        seed.deliver(&merge(5, format!("{ak} e"), "2026-10-11T13:00:00Z"))
            .await;
        assert_eq!(seed.status(at).await, TaskStatus::NeedsYou);
        // The workflow task waits for both PRs, then goes to done.
        seed.deliver(&merge(1, format!("{wk} a"), "2026-10-11T12:00:00Z"))
            .await;
        assert_eq!(
            seed.status(wt).await,
            TaskStatus::InProgress,
            "one PR still open"
        );
        seed.deliver(&merge(2, format!("{wk} b"), "2026-10-11T12:00:00Z"))
            .await;
        assert_eq!(seed.status(wt).await, TaskStatus::Done);
        let activity = seed.activity(wt).await;
        let moved = activity.last().unwrap();
        assert_eq!(moved["kind"], "status");
        assert_eq!(moved["body"]["source"], "pr_automation");
        assert_eq!(moved["body"]["pull_requests"], "#1, #2");
        assert_eq!(moved["body"]["to"], "done");
        assert!(activity.iter().any(|a| a["body"]["action"] == "merged"));
        assert!(activity.iter().any(|a| a["body"]["action"] == "linked"));
    }

    #[tokio::test]
    async fn github_disabled_mirrors_without_linking() {
        let (_dir, seed) = seed().await;
        let wt = &seed.workflow_task;
        let mut off = seed.workspace.clone();
        off.git_settings.github_enabled = false;
        let event = PrEvent::from_github(&github_payload(
            1,
            &format!("{} a", seed.key(wt)),
            "b",
            "open",
            false,
        ))
        .unwrap();
        let mut tx = seed.store.begin().await.unwrap();
        let pr = mirror(&mut tx, &off, &event).await.unwrap().unwrap();
        tx.commit().await.unwrap();
        assert_eq!(seed.linked(wt).await, vec![]);
        let mut tx = seed.store.read().await.unwrap();
        assert_eq!(tx.get_pull_request(pr.id).await.unwrap().number, 1);
    }

    #[tokio::test]
    async fn snapshot_is_kept_only_for_the_current_head() {
        let (_dir, seed) = seed().await;
        let pr = seed
            .deliver(&github_payload(1, "a", "b", "open", false))
            .await
            .unwrap();
        let mut tx = seed.store.begin().await.unwrap();
        let old = pipeline_snapshot("old".into(), "success");
        assert!(!tx.set_snapshot(pr.id, &old).await.unwrap());
        let now = pipeline_snapshot("abc".into(), "failed");
        assert!(tx.set_snapshot(pr.id, &now).await.unwrap());
        let stored = tx.get_pull_request(pr.id).await.unwrap();
        assert!(stored.snapshot_available());
        assert_eq!(stored.checks_rollup_state.as_deref(), Some("FAILURE"));
        assert_eq!((stored.checks_total, stored.checks_failed), (1, 1));
        assert_eq!(pipeline_snapshot("x".into(), "running").checks_pending, 1);
        assert_eq!(
            delivery_head(
                "check_run",
                &json!({ "check_run": { "check_suite": { "head_sha": "s" } } })
            ),
            Some("s".into())
        );
        assert_eq!(
            delivery_head("status", &json!({ "sha": "t" })),
            Some("t".into())
        );
    }

    /// A second workspace (prefix `OPS`) with one open task.
    async fn other_workspace(seed: &Seed) -> (Workspace, Task) {
        let mut tx = seed.store.begin().await.unwrap();
        let workspace = Workspace {
            id: WorkspaceId::generate(),
            name: "Ops".into(),
            slug: "ops".into(),
            description: String::new(),
            context: String::new(),
            logo_hash: Option::None,
            issue_prefix: "OPS".into(),
            issue_counter: 0,
            default_workflow_id: Option::None,
            git_settings: GitSettings::default(),
            created_by: None,
            created_at: chrono::Utc::now(),
        };
        tx.insert_workspace(&workspace).await.unwrap();
        let mut task = Task {
            status: TaskStatus::InProgress,
            ..Task::new(workspace.id, "ops thing")
        };
        task.number = tx.insert_task(&task).await.unwrap();
        tx.commit().await.unwrap();
        (workspace, task)
    }

    #[tokio::test]
    async fn body_keys_link_only_after_a_closing_word_and_only_in_their_workspace() {
        let (_dir, seed) = seed().await;
        let wt = &seed.workflow_task;
        let wk = seed.key(wt);
        let with_body = |number: i64, body: String| {
            let mut p = github_payload(number, "no key here", "topic", "open", false);
            p["pull_request"]["body"] = json!(body);
            p
        };
        // A bare mention in the body is not a claim.
        seed.deliver(&with_body(1, format!("Mentions {wk} in passing")))
            .await;
        assert_eq!(seed.linked(wt).await, vec![]);
        // `fixes sun-12` (lowercase) and `Closes SUN-12` are.
        seed.deliver(&with_body(2, format!("fixes {}", wk.to_lowercase())))
            .await;
        assert_eq!(seed.linked(wt).await, vec![(2, LinkSource::Auto)]);
        seed.deliver(&with_body(3, format!("Done.\n\nCloses {wk}")))
            .await;
        assert_eq!(
            seed.linked(wt).await,
            vec![(2, LinkSource::Auto), (3, LinkSource::Auto)]
        );

        // Ops' key in a PR delivered to Main links nothing there, and Main's
        // key in a PR delivered to Ops links nothing in Main: a workspace
        // only links its own tasks, whatever prefixes the PR names.
        let (ops, ops_task) = other_workspace(&seed).await;
        let ok = format!("OPS-{}", ops_task.number.unwrap());
        seed.deliver(&github_payload(
            4,
            &format!("{ok} and {wk}"),
            "b",
            "open",
            false,
        ))
        .await;
        assert_eq!(seed.linked(&ops_task).await, vec![]);
        assert_eq!(seed.linked(wt).await.len(), 3, "{wk} linked #4 in Main");
        let event = PrEvent::from_github(&github_payload(
            5,
            &format!("{ok} {wk}"),
            "b",
            "open",
            false,
        ))
        .unwrap();
        let mut tx = seed.store.begin().await.unwrap();
        mirror(&mut tx, &ops, &event).await.unwrap().unwrap();
        tx.commit().await.unwrap();
        assert_eq!(seed.linked(&ops_task).await, vec![(5, LinkSource::Title)]);
        assert!(
            !seed.linked(wt).await.iter().any(|(n, _)| *n == 5),
            "Ops' delivery never reaches Main's task"
        );
    }

    #[tokio::test]
    async fn the_merge_rule_stands_down_for_switched_off_closed_done_and_already_moved_tasks() {
        let (_dir, seed) = seed().await;
        let (wt, at) = (&seed.workflow_task, &seed.agent_task);
        let (wk, ak) = (seed.key(wt), seed.key(at));
        let open = |number: i64, title: String| github_payload(number, &title, "b", "open", false);
        let merge = |number: i64, title: String, at: &str| {
            let mut p = github_payload(number, &title, "b", "closed", true);
            p["pull_request"]["updated_at"] = json!(at);
            p
        };
        let moves = |activity: &[Value]| {
            activity
                .iter()
                .filter(|a| a["kind"] == "status" && a["body"]["source"] == "pr_automation")
                .count()
        };

        // The task's own switch off: a merge moves nothing.
        {
            let mut tx = seed.store.begin().await.unwrap();
            tx.set_pr_auto_complete(at.id, true).await.unwrap();
            tx.commit().await.unwrap();
        }
        seed.deliver(&open(1, format!("{ak} a"))).await;
        seed.deliver(&merge(1, format!("{ak} a"), "2026-10-11T12:00:00Z"))
            .await;
        assert_eq!(seed.status(at).await, TaskStatus::InProgress);
        let off = seed.decision(at.id).await;
        assert_eq!(
            (off.state, off.task_disabled),
            (AutoCompleteState::TaskDisabled, true)
        );

        // One PR closed without merging among merged ones: not every PR
        // merged, so the task waits for a person.
        seed.deliver(&open(2, format!("{wk} b"))).await;
        seed.deliver(&open(3, format!("{wk} c"))).await;
        seed.deliver(&merge(2, format!("{wk} b"), "2026-10-11T12:00:00Z"))
            .await;
        let mut closed = github_payload(3, &format!("{wk} c"), "b", "closed", false);
        closed["pull_request"]["updated_at"] = json!("2026-10-11T12:00:00Z");
        closed["pull_request"]["closed_at"] = json!("2026-10-11T12:00:00Z");
        let three = seed.deliver(&closed).await.unwrap();
        assert_eq!(seed.status(wt).await, TaskStatus::InProgress);
        let not_merged = seed.decision(wt.id).await;
        assert_eq!(not_merged.state, AutoCompleteState::NotMerged);
        assert_eq!(not_merged.pull_request_ids, vec![three.id]);

        // A task already done stays done, and nothing is logged for it.
        {
            let mut tx = seed.store.begin().await.unwrap();
            tx.set_task_status_by_merge(wt.id, TaskStatus::Done)
                .await
                .unwrap();
            tx.commit().await.unwrap();
        }
        seed.deliver(&open(4, format!("{wk} d"))).await;
        seed.deliver(&merge(4, format!("{wk} d"), "2026-10-11T13:00:00Z"))
            .await;
        assert_eq!(seed.status(wt).await, TaskStatus::Done);
        assert_eq!(
            seed.decision(wt.id).await.state,
            AutoCompleteState::Terminal
        );
        assert_eq!(moves(&seed.activity(wt).await), 0);

        // The agent task, switch back on: its merged PR moves it once. A
        // person moves it back to work; the same merged PR delivered again
        // (a label, a comment) is not a merge, so it stays.
        {
            let mut tx = seed.store.begin().await.unwrap();
            tx.set_pr_auto_complete(at.id, false).await.unwrap();
            tx.commit().await.unwrap();
        }
        seed.deliver(&open(5, format!("{ak} e"))).await;
        seed.deliver(&merge(5, format!("{ak} e"), "2026-10-11T13:00:00Z"))
            .await;
        assert_eq!(seed.status(at).await, TaskStatus::NeedsYou);
        assert_eq!(moves(&seed.activity(at).await), 1);
        {
            let mut tx = seed.store.begin().await.unwrap();
            tx.set_task_status(at.id, TaskStatus::InProgress)
                .await
                .unwrap();
            tx.commit().await.unwrap();
        }
        seed.deliver(&merge(5, format!("{ak} e"), "2026-10-11T14:00:00Z"))
            .await;
        seed.deliver(&merge(1, format!("{ak} a"), "2026-10-11T14:00:00Z"))
            .await;
        assert_eq!(seed.status(at).await, TaskStatus::InProgress);
        assert_eq!(moves(&seed.activity(at).await), 1, "moved once");
        assert_eq!(
            seed.decision(at.id).await.state,
            AutoCompleteState::AllMerged
        );
    }
}
