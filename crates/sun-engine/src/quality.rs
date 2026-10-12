//! Post-merge quality, an engine extension: did a
//! task's work hold up after its PR merged?
//!
//! - The git host plugin reports the merge: its webhook emits `pr_merged`,
//!   and [`Engine::record_merge`] creates the task's `task_quality` row.
//! - [`Engine::check_quality`] refreshes the follow-up signals: CI of the
//!   merge commit (the plugin's `ci_status`), a revert of it, and fix commits
//!   on the base branch that touch the PR's files within `window_days` (git
//!   on the host, in the task worktree), and bug issues opened after the
//!   merge that name its code (the plugin's `search_issues`).
//! - Bug issues are linked at the merge: the issue whose trigger started the
//!   task and the issues its title and description link.
//! - [`Engine::follow_merges`] drives both: merges as they arrive, and a
//!   re-check of every record whose window is still open.
//!
//! Every change journals an event (`pr_merged`, `quality_checked`) on the
//! task's latest run, so it shows on the timeline.

use std::time::Duration;

use chrono::Utc;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sun_core::host_protocol::{ExecRequest, Worktree};
use sun_core::model::{BugIssue, BugRelation, FixCommit, Task, TaskQuality};
use sun_core::{Error, ModCall, Result, TaskId, Timestamp};
use sun_store::{EventLog, NewEvent, QualityRepo, RunRepo, SettingRepo, TaskRepo, Tx};

use crate::Engine;
use crate::executor::{next, tasks, worktree_of};
use crate::gates::quote;

/// Days after a merge in which fix commits count against it.
pub const DEFAULT_WINDOW_DAYS: u32 = 14;
/// How often open windows are re-checked.
// ponytail: a fixed timer; a `push` webhook trigger could re-check exactly
// when the base branch moves.
const RECHECK: Duration = Duration::from_secs(6 * 3600);
/// The git host trigger that reports a merged PR.
pub const MERGED_TRIGGER: &str = "pr_merged";
/// Search terms per `search_issues` call: the merge sha, then the PR's files.
// ponytail: GitHub search takes at most 5 `OR`s; a PR touching more files is
// searched by its first ones only.
const MAX_TERMS: usize = 6;
/// Bug issues kept per task.
const MAX_BUGS: usize = 20;

/// An issue the plugin's `search_issues` found (plugin data is input).
#[derive(Debug, Deserialize)]
struct FoundIssue {
    key: String,
    title: String,
    url: Option<String>,
    #[serde(default)]
    labels: Vec<String>,
    created_at: Timestamp,
}

/// A merged PR, as the git host plugin's `pr_merged` trigger reports it.
#[derive(Debug, Clone, Deserialize)]
pub struct Merge {
    /// `owner/name`.
    pub repo: String,
    /// PR / MR number.
    pub number: Option<i64>,
    /// Head branch; Sun's are `sun/<KEY>-<slug>`.
    pub branch: String,
    /// Branch it merged into.
    pub base: String,
    /// The merge (or squash) commit.
    pub merge_sha: String,
    /// When it merged.
    pub merged_at: Timestamp,
}

impl Merge {
    /// Plugin data is input: keep only what is safe to put in a git command.
    fn validate(&self) -> Result<()> {
        let sha_ok = (7..=64).contains(&self.merge_sha.len())
            && self.merge_sha.bytes().all(|b| b.is_ascii_hexdigit());
        let ref_ok = |r: &str| {
            !r.is_empty()
                && r.len() <= 255
                && !r.starts_with('-')
                && !r.contains("..")
                && !r.chars().any(|c| c.is_whitespace() || c.is_control())
        };
        if !sha_ok || !ref_ok(&self.base) || self.repo.is_empty() || self.repo.len() > 255 {
            return Err(Error::InvalidInput(format!(
                "pr_merged: bad repo, base or merge_sha ({}, {}, {})",
                self.repo, self.base, self.merge_sha
            )));
        }
        Ok(())
    }
}

/// Post-merge quality over every merged task (`GET /quality/metrics`).
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct QualityMetrics {
    /// Tasks whose PR merged.
    pub merged: u32,
    /// Merged tasks with a verdict ([`TaskQuality::held_up`] is `Some`).
    pub decided: u32,
    /// Held up: green CI, no revert, no fix commits.
    pub held_up: u32,
    /// CI failed on the merge commit.
    pub ci_failed: u32,
    /// Reverted.
    pub reverted: u32,
    /// Needed fix commits within the window.
    pub with_fixes: u32,
    /// Had bug issues reported against them after the merge.
    pub with_bugs: u32,
    /// `held_up / decided`, when anything is decided.
    pub held_up_rate: Option<f64>,
}

impl QualityMetrics {
    fn of(records: &[TaskQuality]) -> Self {
        let mut m = Self::default();
        for q in records {
            m.merged += 1;
            match q.held_up() {
                Some(true) => (m.decided, m.held_up) = (m.decided + 1, m.held_up + 1),
                Some(false) => m.decided += 1,
                None => {}
            }
            m.ci_failed += u32::from(q.ci_state.as_deref() == Some("failure"));
            m.reverted += u32::from(q.reverted_by.is_some());
            m.with_fixes += u32::from(!q.fix_commits.is_empty());
            m.with_bugs += u32::from(q.regressions().next().is_some());
        }
        m.held_up_rate = (m.decided > 0).then(|| f64::from(m.held_up) / f64::from(m.decided));
        m
    }
}

impl Engine {
    /// The task's quality record, if its PR merged.
    pub async fn quality(&self, task_id: TaskId) -> Result<Option<TaskQuality>> {
        let mut tx = self.store.read().await?;
        tx.get_task(task_id).await?;
        Ok(tx.quality_of(task_id).await?)
    }

    /// Totals over every merged task.
    pub async fn quality_metrics(&self) -> Result<QualityMetrics> {
        let records = self.store.read().await?.quality_records().await?;
        Ok(QualityMetrics::of(&records))
    }

    /// Records that `merge` landed (git host plugin `mod_name`). `Ok(None)`
    /// when its branch is not a task of this ecosystem.
    pub async fn record_merge(&self, mod_name: &str, merge: Merge) -> Result<Option<TaskQuality>> {
        merge.validate()?;
        let mut tx = self.store.begin().await?;
        let Some(task_id) = tasks::task_of_branch(&mut tx, &merge.branch).await? else {
            return Ok(None);
        };
        let task = tx.get_task(task_id).await?;
        let trigger_key = format!("{}{task_id}", crate::triggers::TRIGGER_OF);
        let trigger = tx.setting(&trigger_key).await?;
        let bug_issues = linked_issues(&task, trigger.as_ref(), &merge.repo, merge.number);
        let quality = TaskQuality {
            task_id,
            mod_name: mod_name.to_owned(),
            repo: merge.repo,
            pr_number: merge.number,
            base: merge.base,
            merge_sha: merge.merge_sha,
            merged_at: merge.merged_at,
            window_days: DEFAULT_WINDOW_DAYS,
            ci_state: None,
            reverted_by: None,
            fix_commits: Vec::new(),
            bug_issues,
            checked_at: None,
        };
        tx.save_quality(&quality).await?;
        let linked: Vec<&str> = quality.bug_issues.iter().map(|b| b.key.as_str()).collect();
        let payload = json!({
            "repo": quality.repo,
            "number": quality.pr_number,
            "base": quality.base,
            "merge_sha": quality.merge_sha,
            "bug_issues": linked,
        });
        let run = journal(&mut tx, task_id, "pr_merged", payload).await?;
        tx.commit().await?;
        if let Some(run_id) = run {
            self.recheck(run_id).await;
        }
        Ok(Some(quality))
    }

    /// Refreshes the task's follow-up signals. `InvalidInput` when its PR
    /// has not merged. CI comes from the plugin; a failed CI call keeps the
    /// previous state (the git signals still update).
    pub async fn check_quality(&self, task_id: TaskId) -> Result<TaskQuality> {
        let (mut quality, worktree) = {
            let mut tx = self.store.read().await?;
            let quality = tx.quality_of(task_id).await?.ok_or_else(|| {
                Error::InvalidInput(format!("task {task_id} has no merged PR yet"))
            })?;
            (quality, worktree_of(&mut tx, task_id).await?)
        };
        let ci = ModCall {
            mod_name: quality.mod_name.clone(),
            method: "ci_status".into(),
            params: json!({ "repo": quality.repo, "ref": quality.merge_sha }),
            idem_key: None,
        };
        let ci = crate::vcs::call_git_host(&self.store, &self.vcs, &*self.mods, Some(task_id), ci);
        if let Ok(Ok(reply)) = tokio::time::timeout(Duration::from_secs(60), ci).await
            && let Some(state @ ("success" | "failure" | "pending" | "none")) =
                reply["state"].as_str()
        {
            quality.ci_state = Some(state.to_owned());
        }
        // ponytail: a multi-repo task keeps one record (its latest merge)
        // and only CI; per-repo records when multi-repo tasks need revert/fix signals.
        let mut files = Vec::new();
        if !matches!(worktree, Worktree::Repos { .. }) {
            let cwd = self.hosts.prepare_worktree(self.host, worktree).await?;
            let request = ExecRequest {
                cwd,
                command: script(&quality.base, &quality.merge_sha),
                env: Default::default(),
                timeout_s: 120,
            };
            let out = self.hosts.exec(self.host, request).await?;
            if out.code != Some(0) {
                return Err(Error::Unavailable(format!(
                    "quality check on the host: {}",
                    out.output.trim()
                )));
            }
            let signals = parse(&out.output, &quality);
            quality.reverted_by = signals.0;
            quality.fix_commits = signals.1;
            files = signals.2;
        }
        if let Some(found) = self.regressions(&quality, &files).await {
            quality
                .bug_issues
                .retain(|b| b.relation != BugRelation::Regression);
            quality.bug_issues.extend(found);
        }
        quality.checked_at = Some(Utc::now());

        let mut tx = self.store.begin().await?;
        tx.save_quality(&quality).await?;
        let payload = json!({
            "held_up": quality.held_up(),
            "ci_state": quality.ci_state,
            "reverted_by": quality.reverted_by,
            "fix_commits": quality.fix_commits.len(),
            "regressions": quality.regressions().count(),
        });
        journal(&mut tx, task_id, "quality_checked", payload).await?;
        tx.commit().await?;
        Ok(quality)
    }

    /// Runs until the plugins' event stream ends: records each `pr_merged`
    /// trigger and checks it at once, and re-checks every record whose
    /// window is still open every [`RECHECK`]. The caller subscribes the git
    /// host plugins to [`MERGED_TRIGGER`].
    pub async fn follow_merges(self) {
        let mut events = self.mods.events();
        let mut tick = tokio::time::interval(RECHECK);
        loop {
            tokio::select! {
                event = next(&mut events) => {
                    let Some(event) = event else { break };
                    if event.topic != MERGED_TRIGGER {
                        continue;
                    }
                    let merge = match serde_json::from_value::<Merge>(event.payload) {
                        Ok(merge) => merge,
                        Err(e) => {
                            eprintln!("sun: quality: pr_merged from {}: {e}", event.mod_name);
                            continue;
                        }
                    };
                    match self.record_merge(&event.mod_name, merge).await {
                        Ok(Some(q)) => self.report(self.check_quality(q.task_id).await),
                        Ok(None) => {}
                        Err(e) => self.report(Err(e)),
                    }
                }
                _ = tick.tick() => self.recheck_open().await,
            }
        }
    }

    /// Bug issues opened within the window that name the merged code: the
    /// plugin's `search_issues {repo, since, terms}` over the merge sha and
    /// the PR's `files`. `None` when the call fails (the previous ones stay).
    async fn regressions(&self, quality: &TaskQuality, files: &[String]) -> Option<Vec<BugIssue>> {
        let terms: Vec<&str> = quality
            .merge_sha
            .get(..7)
            .into_iter()
            .chain(files.iter().map(String::as_str))
            .take(MAX_TERMS)
            .collect();
        let call = ModCall {
            mod_name: quality.mod_name.clone(),
            method: "search_issues".into(),
            params: json!({
                "repo": quality.repo,
                "since": quality.merged_at.to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
                "terms": terms,
            }),
            idem_key: None,
        };
        let task = Some(quality.task_id);
        let call = crate::vcs::call_git_host(&self.store, &self.vcs, &*self.mods, task, call);
        let reply = tokio::time::timeout(Duration::from_secs(60), call)
            .await
            .ok()?
            .ok()?;
        let found: Vec<FoundIssue> = serde_json::from_value(reply["issues"].clone()).ok()?;
        let end = quality.merged_at + chrono::Duration::days(i64::from(quality.window_days));
        let mut bugs: Vec<BugIssue> = Vec::new();
        for issue in found {
            if bugs.len() >= MAX_BUGS {
                break;
            }
            let known = quality
                .bug_issues
                .iter()
                .filter(|b| b.relation != BugRelation::Regression)
                .chain(&bugs)
                .any(|b| b.key == issue.key);
            let in_window = issue.created_at >= quality.merged_at && issue.created_at <= end;
            if known || !in_window || !is_bug(&issue.labels, &issue.title) {
                continue;
            }
            bugs.push(BugIssue {
                relation: BugRelation::Regression,
                key: issue.key.chars().take(200).collect(),
                title: Some(issue.title.chars().take(200).collect()),
                url: issue.url.filter(|u| is_web_url(u)),
                opened_at: Some(issue.created_at),
            });
        }
        Some(bugs)
    }

    async fn recheck_open(&self) {
        let records = match self.store.read().await {
            Ok(mut tx) => tx.quality_records().await.unwrap_or_default(),
            Err(_) => return,
        };
        let now = Utc::now();
        for q in records {
            // One extra day: commits made at the window's end still land.
            let end = q.merged_at + chrono::Duration::days(i64::from(q.window_days) + 1);
            if now <= end {
                self.report(self.check_quality(q.task_id).await);
            }
        }
    }

    // ponytail: background failures go to stderr; journal them when someone
    // needs them on the timeline.
    fn report(&self, result: Result<TaskQuality>) {
        if let Err(e) = result {
            eprintln!("sun: quality: {e}");
        }
    }
}

/// `kind` on the task's latest run, if it has one; returns that run.
pub(crate) async fn journal(
    tx: &mut Tx,
    task_id: TaskId,
    kind: &str,
    payload: serde_json::Value,
) -> Result<Option<sun_core::RunId>> {
    let run = tx.runs_of_task(task_id).await?.pop();
    if let Some(run) = &run {
        tx.append_event(NewEvent::new(run.id, None, kind, payload))
            .await?;
    }
    Ok(run.map(|r| r.id))
}

/// Prints `revert <sha>` (empty when not reverted), the PR's files as
/// `file\t<path>`, then every non-merge commit on `origin/<base>` after the
/// merge that touches them, as `<sha>\t<unix time>\t<subject>`.
// ponytail: the PR's files are `merge^1..merge`, exact for merge and squash
// commits; a rebase-merge only counts its last commit's files.
fn script(base: &str, sha: &str) -> String {
    let base = quote(base);
    let sha = quote(sha);
    format!(
        "set -e\ngit fetch -q origin {base}\nm={sha}\ngit cat-file -e \"$m^{{commit}}\"\n\
         echo \"revert $(git log -1 --format=%H --grep=\"This reverts commit $m\" \"origin/\"{base} --)\"\n\
         git -c core.quotePath=false diff --name-only \"$m^1\" \"$m\" | \
         while IFS= read -r f; do printf 'file\\t%s\\n' \"$f\"; done\n\
         if [ -n \"$(git diff --name-only \"$m^1\" \"$m\")\" ]; then\n\
         git -c core.quotePath=false diff --name-only -z \"$m^1\" \"$m\" | xargs -0 git log \
         --no-merges --format='%H%x09%ct%x09%s' \"$m..origin/\"{base} --\nfi"
    )
}

/// `(reverted_by, fix_commits, files)` from [`script`]'s output: fix commits
/// are those within the window whose subject reads like a fix.
fn parse(output: &str, quality: &TaskQuality) -> (Option<String>, Vec<FixCommit>, Vec<String>) {
    let mut lines = output.lines();
    let reverted_by = lines
        .next()
        .and_then(|l| l.strip_prefix("revert "))
        .map(str::trim)
        .filter(|sha| !sha.is_empty())
        .map(str::to_owned);
    let (files, commits): (Vec<&str>, Vec<&str>) = lines.partition(|l| l.starts_with("file\t"));
    let files = files
        .iter()
        .filter_map(|l| l.strip_prefix("file\t"))
        .map(str::to_owned)
        .collect();
    let end = quality.merged_at + chrono::Duration::days(i64::from(quality.window_days));
    let mut fixes: Vec<FixCommit> = commits
        .into_iter()
        .filter_map(|line| {
            let mut parts = line.splitn(3, '\t');
            let sha = parts.next()?.to_owned();
            let at = chrono::DateTime::from_timestamp(parts.next()?.parse().ok()?, 0)?;
            let subject = parts.next()?.to_owned();
            Some(FixCommit { sha, subject, at })
        })
        .filter(|c| c.at >= quality.merged_at && c.at <= end)
        .filter(|c| Some(&c.sha) != reverted_by.as_ref() && is_fix(&c.subject))
        .collect();
    fixes.sort_by_key(|c| c.at);
    fixes.dedup_by(|a, b| a.sha == b.sha);
    (reverted_by, fixes, files)
}

/// Does an issue read like a bug? A bug-like label (or Jira issue type,
/// which mods report as a label), else a fix-like title.
fn is_bug(labels: &[String], title: &str) -> bool {
    labels.iter().any(|l| {
        let l = l.to_lowercase();
        ["bug", "regression", "defect", "incident"]
            .iter()
            .any(|w| l.contains(w))
    }) || is_fix(title)
}

/// `http(s)://` only: links end up in the UI as `href`s.
fn is_web_url(url: &str) -> bool {
    (url.starts_with("https://") || url.starts_with("http://")) && url.len() <= 2048
}

/// The issue that started the task (its trigger's `url`) and the issues its
/// title and description link (issue URLs, `#12` in `repo`), each key once.
fn linked_issues(
    task: &Task,
    trigger: Option<&Value>,
    repo: &str,
    pr: Option<i64>,
) -> Vec<BugIssue> {
    let mut out: Vec<BugIssue> = Vec::new();
    let mut add = |relation, key: String, title: Option<String>, url: Option<String>| {
        if out.len() < MAX_BUGS && !out.iter().any(|b| b.key == key) {
            let opened_at = None;
            out.push(BugIssue {
                relation,
                key,
                title,
                url,
                opened_at,
            });
        }
    };
    if let Some(trigger) = trigger
        && let Some(url) = trigger["url"].as_str()
        && let Some(key) = issue_key(url)
    {
        let title = trigger["title"]
            .as_str()
            .map(|t| t.chars().take(200).collect());
        add(BugRelation::Origin, key, title, Some(url.to_owned()));
    }
    let text = format!("{}\n{}", task.title, task.body);
    for word in text.split_whitespace() {
        let word = word.trim_matches(|c: char| "()[]<>\"',.;:!?`*".contains(c));
        if let Some(key) = issue_key(word) {
            add(BugRelation::Referenced, key, None, Some(word.to_owned()));
        } else if let Some(n) = word.strip_prefix('#')
            && let Ok(n) = n.parse::<u32>()
            && pr != Some(i64::from(n))
        {
            add(BugRelation::Referenced, format!("{repo}#{n}"), None, None);
        }
    }
    out
}

/// `owner/repo#12` for a GitHub, GitLab or Gitea issue URL, the key of a Jira
/// `…/browse/APP-4` URL; `None` for anything else.
fn issue_key(url: &str) -> Option<String> {
    if !is_web_url(url) {
        return None;
    }
    let (_, path) = url.split_once("://")?.1.split_once('/')?;
    let path = path.split(['?', '#']).next()?;
    let parts: Vec<&str> = path.split('/').filter(|p| !p.is_empty()).collect();
    let number = |n: &str| n.parse::<u32>().is_ok();
    let jira = |k: &str| {
        k.split_once('-').is_some_and(|(project, n)| {
            project.starts_with(|c: char| c.is_ascii_uppercase())
                && project
                    .chars()
                    .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit())
                && number(n)
        })
    };
    match parts.as_slice() {
        [.., "browse", key] if jira(key) => Some((*key).to_owned()),
        [repo @ .., "-", "issues", n] | [repo @ .., "issues", n]
            if repo.len() >= 2 && number(n) =>
        {
            Some(format!("{}#{n}", repo.join("/")))
        }
        _ => None,
    }
}

/// Does a commit subject read like a fix? Conventional `fix:` and the usual words.
fn is_fix(subject: &str) -> bool {
    subject
        .to_lowercase()
        .split(|c: char| !c.is_ascii_alphanumeric())
        .any(|w| {
            matches!(
                w,
                "fix" | "fixes" | "fixed" | "hotfix" | "bugfix" | "bug" | "regression" | "revert"
            )
        })
}

#[cfg(test)]
mod tests;
