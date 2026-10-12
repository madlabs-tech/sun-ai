//! Multi-repo tasks: one task binds N repos.
//!
//! - **Worktree:** a session dir `work/<task_id>` holding one worktree per
//!   repo at `<folder>/`, each on the task's branch (`checkouts`). The
//!   agent works in the session dir and is told the layout (`context`).
//! - **Gates** run per repo (`gates`), `integration` once in the session dir.
//! - **PRs:** one per changed repo (`push`, `outbox`), each body linking the
//!   others in merge order (`links`).
//! - **Merge order** is the run's `repo_ids` order. The `merge_order` gate
//!   waits until every PR merged and fails when one merged before an
//!   earlier one ([`Order`]); merges come from the git host's `pr_merged`
//!   trigger, CI of the next PR from its `ci_status`. While it waits, the
//!   commit status `sun/merge-order` is `success` on the next PR and
//!   `pending` on later ones (`statuses`, through the outbox), so a git host
//!   whose branch protection requires it blocks out-of-order merges. Once
//!   every PR is open, each gets a comment listing all PR URLs in merge
//!   order (`link_comments`).
//! - **Cleanup:** the host removes the session dir when the run is done or
//!   cancelled (`Effect::RemoveWorktree`).

use serde::Serialize;
use serde_json::{Value, json};
use sun_core::host_protocol::{RepoCheckout, Worktree};
use sun_core::model::{OutboxItem, OutboxStatus, Repo};
use sun_core::{IdemKey, OutboxId, RepoId, Result, RunId, StepKey, TaskId};
use sun_store::{EventLog, OutboxRepo, RepoRepo, RunRepo, Tx};

use crate::Engine;
use crate::executor::kind::OUTBOX_DELIVERED;
use crate::outbox::repo_slug;

/// Each repo's folder in the session dir: the last part of its slug, made
/// safe for the host (`[A-Za-z0-9._-]`) and unique (`api`, `api-2`); every
/// one on `branch`.
pub(crate) fn checkouts(repos: &[Repo], branch: &str) -> Vec<RepoCheckout> {
    let mut out: Vec<RepoCheckout> = Vec::new();
    for repo in repos {
        let slug = repo_slug(&repo.url);
        let last = slug.rsplit('/').next().unwrap_or_default();
        let clean: String = last
            .chars()
            .map(|c| match c {
                'a'..='z' | 'A'..='Z' | '0'..='9' | '.' | '_' | '-' => c,
                _ => '-',
            })
            .collect();
        let clean = clean.trim_start_matches(['.', '-']);
        let base = if clean.is_empty() { "repo" } else { clean };
        let base: String = base.chars().take(90).collect();
        let mut name = base.clone();
        let mut n = 2;
        while out.iter().any(|c| c.name == name) {
            name = format!("{base}-{n}");
            n += 1;
        }
        out.push(RepoCheckout {
            name,
            url: repo.url.clone(),
            base: Some(repo.default_branch.clone()),
            branch: branch.to_owned(),
        });
    }
    out
}

/// What the agent is told about a multi-repo worktree (P0 of its pack, so
/// the envelope and `.sun/context/pack.md` carry it). `None` otherwise.
pub(crate) fn context(worktree: &Worktree) -> Option<String> {
    let Worktree::Repos { repos, .. } = worktree else {
        return None;
    };
    let mut text = format!(
        "## Repos\n\nThis task spans {} repos. Your working directory holds one git \
         worktree per repo, each on branch `{}`. Change, test and commit inside each \
         folder; Sun pushes every changed repo and opens one PR per repo. Merge order:\n",
        repos.len(),
        worktree.branch().unwrap_or_default()
    );
    for (i, repo) in repos.iter().enumerate() {
        text.push_str(&format!(
            "{}. `{}/`: {} (base `{}`)\n",
            i + 1,
            repo.name,
            repo_slug(&repo.url),
            repo.base.as_deref().unwrap_or("default")
        ));
    }
    Some(text)
}

/// The cross-links in `this` repo's PR body: every PR of the change, in
/// merge order, by repo and branch.
pub(crate) fn links(repos: &[Repo], this: &Repo, branch: &str) -> String {
    if repos.len() < 2 {
        return String::new();
    }
    let mut text = format!(
        "This change spans {} repos. Merge the PRs of branch `{branch}` in this order:\n",
        repos.len()
    );
    for (i, repo) in repos.iter().enumerate() {
        let slug = repo_slug(&repo.url);
        let name = match web_url(&repo.url) {
            Some(url) => format!("[{slug}]({url})"),
            None => slug,
        };
        let here = if repo.id == this.id { " (this PR)" } else { "" };
        text.push_str(&format!("{}. {name}{here}\n", i + 1));
    }
    text.push_str("\n---\n");
    text
}

/// The browsable URL of an `https://` clone URL.
fn web_url(url: &str) -> Option<&str> {
    url.starts_with("https://").then_some(())?;
    Some(
        url.strip_suffix(".git")
            .unwrap_or(url)
            .trim_end_matches('/'),
    )
}

/// One repo of a task and its PR, in merge order (`GET /tasks/{id}`).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct RepoPr {
    /// The repo.
    pub repo_id: RepoId,
    /// `owner/name`.
    pub slug: String,
    /// Clone URL.
    pub url: String,
    /// 1-based merge position.
    pub position: usize,
    /// Its PR once opened: `{number, url}` as the git host answered.
    pub pr: Option<Value>,
    /// `pending` (no PR yet, or the repo didn't change), `open`, `merged`, or
    /// `closed` (closed without merging; reopening makes it `open` again).
    pub status: &'static str,
    /// Event `seq` of the merge, for ordering.
    #[serde(skip)]
    pub merged_seq: Option<i64>,
}

/// The run's repos with their PRs and merges, as journalled on it.
pub(crate) async fn task_repos(tx: &mut Tx, run_id: RunId) -> Result<Vec<RepoPr>> {
    let run = tx.get_run(run_id).await?;
    let mut out = Vec::new();
    for (i, id) in run.repo_ids.iter().enumerate() {
        let repo = tx.get_repo(*id).await?;
        out.push(RepoPr {
            repo_id: repo.id,
            slug: repo_slug(&repo.url),
            url: repo.url,
            position: i + 1,
            pr: None,
            status: "pending",
            merged_seq: None,
        });
    }
    let single = out.len() == 1;
    let mut after = 0;
    loop {
        let page = tx.events_after(Some(run_id), after, 500).await?;
        let Some(last) = page.last() else { break };
        after = last.seq;
        for event in page {
            let p = &event.payload;
            // Rows from before PRs carried their repo match a single repo.
            let of = |r: &RepoPr| p["repo"].as_str().map_or(single, |s| s == r.slug);
            let pr_opened = event.kind == OUTBOX_DELIVERED
                && p["target"]
                    .as_str()
                    .is_some_and(|t| t.ends_with(".open_pr"));
            for repo in out.iter_mut().filter(|r| of(r)) {
                if pr_opened {
                    repo.pr = Some(json!({
                        "number": p["detail"]["number"],
                        "url": p["detail"]["url"],
                        "head_sha": p["detail"]["head_sha"],
                    }));
                    if repo.merged_seq.is_none() {
                        repo.status = "open";
                    }
                } else if event.kind == crate::quality::MERGED_TRIGGER {
                    repo.merged_seq = Some(event.seq);
                    repo.status = "merged";
                } else if event.kind == crate::triggers::PR_STATE_TRIGGER
                    && repo.merged_seq.is_none()
                {
                    repo.status = match p["state"].as_str() {
                        Some("closed") => "closed",
                        _ => "open",
                    };
                }
            }
        }
    }
    Ok(out)
}

/// Where a multi-repo change stands against its merge order.
#[derive(Debug, PartialEq)]
pub(crate) enum Order {
    /// Every PR merged, in order.
    Done,
    /// `next` (an index into the PRs) is the one to merge now.
    Waiting { next: usize },
    /// A PR merged before an earlier one: why.
    Broken(String),
}

/// Checks the merges of the repos that have a PR against their order.
pub(crate) fn merge_order(prs: &[&RepoPr]) -> Order {
    let mut gap: Option<usize> = None;
    let mut last: Option<(i64, &str)> = None;
    for (i, repo) in prs.iter().enumerate() {
        match repo.merged_seq {
            None => {
                gap.get_or_insert(i);
            }
            Some(seq) => {
                if let Some(g) = gap {
                    return Order::Broken(format!(
                        "{} merged before {}: merge order is broken",
                        repo.slug, prs[g].slug
                    ));
                }
                if let Some((prev, slug)) = last
                    && seq < prev
                {
                    return Order::Broken(format!(
                        "{} merged before {slug}: merge order is broken",
                        repo.slug
                    ));
                }
                last = Some((seq, &repo.slug));
            }
        }
    }
    match gap {
        Some(next) => Order::Waiting { next },
        None => Order::Done,
    }
}

/// The commit status the git host checks before a merge: with branch
/// protection requiring it, a PR can't merge before its predecessors.
pub(crate) const STATUS_CONTEXT: &str = "sun/merge-order";

/// `slug#number` of a PR.
pub(crate) fn pr_name(r: &RepoPr) -> String {
    let number = r.pr.as_ref().map_or(Value::Null, |p| p["number"].clone());
    format!("{}#{number}", r.slug)
}

/// `<mod>.set_status` rows for [`STATUS_CONTEXT`] while the PRs wait for
/// their merges: `success` on the next PR to merge, `pending` on every later
/// one. Keys hold the PR's position, its state and the next PR, so a
/// re-check enqueues nothing new and each merge moves the statuses on. PRs
/// whose head sha is unknown are skipped.
// ponytail: statuses go on the head sha `open_pr` returned; a later push to
// the branch needs the PR re-opened (or a re-check) to carry them over.
pub(crate) fn statuses(
    run_id: RunId,
    step_key: &StepKey,
    mod_name: &str,
    prs: &[RepoPr],
) -> Vec<OutboxItem> {
    let prs: Vec<&RepoPr> = prs.iter().filter(|r| r.pr.is_some()).collect();
    let Order::Waiting { next } = merge_order(&prs) else {
        return Vec::new();
    };
    let first = prs[next];
    let mut rows = Vec::new();
    for (i, r) in prs.iter().enumerate().skip(next) {
        let pr = r.pr.as_ref().unwrap_or(&Value::Null);
        let Some(sha) = pr["head_sha"].as_str() else {
            continue;
        };
        let (state, description, target_url) = match i == next {
            true => ("success", "Next to merge".to_owned(), None),
            false => (
                "pending",
                format!("Merge {} first", pr_name(first)),
                first.pr.as_ref().map(|p| p["url"].clone()),
            ),
        };
        let mut params = json!({
            "repo": r.slug, "sha": sha, "state": state, "context": STATUS_CONTEXT,
            "description": description.chars().take(140).collect::<String>(),
        });
        if let Some(url) = target_url.filter(|u| u.as_str().is_some_and(|u| u.starts_with("http")))
        {
            params["target_url"] = url;
        }
        let action = format!("status:{}:{state}:{}", r.position, first.position);
        rows.push(row(
            run_id,
            step_key,
            &action,
            format!("{mod_name}.set_status"),
            params,
        ));
    }
    rows
}

/// One `<mod>.comment` row per PR, posted once all PRs are open: every PR
/// of the change by URL, in merge order (the bodies could only name repos
/// and branches). Nothing for fewer than two PRs.
// ponytail: keyed per run; a retried run that finds the same PRs posts the
// list again. Key by task if that gets noisy.
pub(crate) fn link_comments(
    run_id: RunId,
    step_key: &StepKey,
    mod_name: &str,
    prs: &[RepoPr],
) -> Vec<OutboxItem> {
    let prs: Vec<&RepoPr> = prs.iter().filter(|r| r.pr.is_some()).collect();
    if prs.len() < 2 {
        return Vec::new();
    }
    let line = |i: usize, r: &RepoPr| {
        let url = r.pr.as_ref().and_then(|p| p["url"].as_str());
        match url {
            Some(url) => format!("{}. [{}]({url})", i + 1, pr_name(r)),
            None => format!("{}. {}", i + 1, pr_name(r)),
        }
    };
    prs.iter()
        .filter_map(|this| {
            let number = this.pr.as_ref()?["number"].as_u64()?;
            let mut body = format!(
                "This change spans {} pull requests. Merge them in this order:\n",
                prs.len()
            );
            for (i, r) in prs.iter().enumerate() {
                let here = if r.repo_id == this.repo_id {
                    " (this PR)"
                } else {
                    ""
                };
                body.push_str(&format!("{}{here}\n", line(i, r)));
            }
            let params = json!({ "repo": this.slug, "number": number, "body": body });
            let action = format!("links:{}", this.position);
            Some(row(
                run_id,
                step_key,
                &action,
                format!("{mod_name}.comment"),
                params,
            ))
        })
        .collect()
}

fn row(
    run_id: RunId,
    step_key: &StepKey,
    action: &str,
    target: String,
    payload: Value,
) -> OutboxItem {
    OutboxItem {
        id: OutboxId::generate(),
        run_id: Some(run_id),
        idem_key: IdemKey::new(run_id, step_key, action),
        target,
        payload,
        status: OutboxStatus::Pending,
        result: None,
    }
}

/// Enqueues `rows` in the caller's transaction; `true` when one is new
/// (wake the outbox).
pub(crate) async fn enqueue(tx: &mut Tx, rows: Vec<OutboxItem>) -> Result<bool> {
    let mut added = false;
    for item in rows {
        added |= tx.enqueue(&item).await?;
    }
    Ok(added)
}

impl Engine {
    /// The task's latest run's repos in merge order with each one's PR and
    /// its status; empty before the first run.
    pub async fn task_repos(&self, task_id: TaskId) -> Result<Vec<RepoPr>> {
        let mut tx = self.store.read().await?;
        match tx.runs_of_task(task_id).await?.pop() {
            Some(run) => task_repos(&mut tx, run.id).await,
            None => Ok(Vec::new()),
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use serde_json::json;

    use super::*;

    fn repo(url: &str) -> Repo {
        Repo {
            id: RepoId::generate(),
            workspace_id: sun_core::WorkspaceId::MAIN,
            url: url.into(),
            default_branch: "main".into(),
            gate_config: json!({}),
            description: String::new(),
            hook_id: None,
        }
    }

    fn pr(slug: &str, merged_seq: Option<i64>) -> RepoPr {
        RepoPr {
            repo_id: RepoId::generate(),
            slug: slug.into(),
            url: String::new(),
            position: 0,
            pr: Some(json!({"number": 1})),
            status: "open",
            merged_seq,
        }
    }

    #[test]
    fn multi_repo_folders_are_safe_and_unique() {
        let repos = [
            repo("https://github.com/acme/api.git"),
            repo("git@gitlab.com:other/api.git"),
            repo("/srv/git/.hidden web"),
        ];
        let checkouts = checkouts(&repos, "sun/SUN-3-x");
        let names: Vec<&str> = checkouts.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, ["api", "api-2", "hidden-web"]);
        assert!(checkouts.iter().all(|c| c.branch == "sun/SUN-3-x"));
    }

    #[test]
    fn multi_repo_links_list_every_pr_in_merge_order() {
        let repos = [
            repo("https://github.com/acme/api.git"),
            repo("git@github.com:acme/web.git"),
        ];
        let text = links(&repos, &repos[1], "sun/SUN-3-x");
        assert!(text.contains("`sun/SUN-3-x`"), "{text}");
        assert!(
            text.contains("1. [acme/api](https://github.com/acme/api)\n2. acme/web (this PR)"),
            "{text}"
        );
        assert_eq!(links(&repos[..1], &repos[0], "sun/SUN-3-x"), "");
    }

    #[test]
    fn multi_repo_statuses_follow_the_merges_and_links_list_every_pr() {
        let mut prs: Vec<RepoPr> = ["a/api", "a/web", "a/app"]
            .iter()
            .enumerate()
            .map(|(i, s)| RepoPr {
                position: i + 1,
                pr: Some(json!({
                    "number": i + 1, "url": format!("https://x/{}", i + 1), "head_sha": format!("abc{i}def"),
                })),
                ..pr(s, None)
            })
            .collect();
        let run = RunId::generate();
        let step = StepKey::new("merged").unwrap();
        let rows = |prs: &[RepoPr]| -> Vec<(String, Value)> {
            statuses(run, &step, "gitea", prs)
                .into_iter()
                .map(|r| {
                    assert_eq!(r.target, "gitea.set_status");
                    (r.idem_key.as_str().rsplit(':').take(4).collect(), r.payload)
                })
                .collect()
        };

        let first = rows(&prs);
        let states: Vec<&Value> = first.iter().map(|(_, p)| &p["state"]).collect();
        assert_eq!(states, ["success", "pending", "pending"]);
        assert_eq!(first[1].1["description"], "Merge a/api#1 first");
        assert_eq!(first[1].1["target_url"], "https://x/1");
        assert_eq!(first[2].1["sha"], "abc2def");
        assert_eq!(first[0].1["context"], STATUS_CONTEXT);

        prs[0].merged_seq = Some(7);
        prs[2].pr.as_mut().unwrap()["head_sha"] = Value::Null;
        let second = rows(&prs);
        assert_eq!(second.len(), 1, "merged and sha-less PRs get none");
        assert_eq!(
            (&second[0].1["repo"], &second[0].1["state"]),
            (&json!("a/web"), &json!("success"))
        );
        // New keys, so the outbox sends the change.
        assert!(second.iter().all(|k| !first.contains(k)));

        prs[1].merged_seq = Some(8);
        prs[2].merged_seq = Some(9);
        assert!(rows(&prs).is_empty());

        let links = link_comments(run, &step, "github", &prs);
        assert_eq!(links.len(), 3);
        let body = links[1].payload["body"].as_str().unwrap();
        assert!(
            body.contains("1. [a/api#1](https://x/1)\n2. [a/web#2](https://x/2) (this PR)\n3."),
            "{body}"
        );
        assert_eq!(links[1].target, "github.comment");
        assert_eq!(links[1].payload["number"], 2);
        assert!(links[1].idem_key.as_str().ends_with(":merged:links:2"));
        assert!(link_comments(run, &step, "github", &prs[..1]).is_empty());
    }

    /// A PR closed without merging is `closed`, a reopened one `open` again;
    /// a merged PR stays `merged`.
    #[tokio::test(flavor = "multi_thread")]
    async fn pr_closed_without_merging_then_reopened_then_merged() {
        use std::time::Duration;

        use sun_core::{HostId, ModEvent};
        use sun_store::{NewEvent, Store};

        use crate::gates::tests::{LocalHost, mods, seed_with};

        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).await.unwrap();
        let config = crate::EngineConfig {
            host: HostId::generate(),
            adapter: "fake".into(),
            delivery_timeout: Duration::from_secs(5),
            watchdog: crate::WatchdogConfig::default(),
        };
        let host = LocalHost::new(dir.path(), |_, _| {});
        let (engine, _task) = Engine::start(store.clone(), host, mods(json!({})), config)
            .await
            .unwrap();
        let app = repo("https://github.com/acme/app.git");
        let mut tx = store.begin().await.unwrap();
        tx.insert_repo(&app).await.unwrap();
        tx.commit().await.unwrap();
        let yaml = "name: q\nsteps:\n  - id: a\n    type: agent\n    prompt: x\n";
        let run = seed_with(&store, yaml, vec![app.id]).await;
        let task_id = engine.run(run).await.unwrap().task_id;
        let append = |kind: &'static str, payload: Value| {
            let store = store.clone();
            async move {
                let mut tx = store.begin().await.unwrap();
                tx.append_event(NewEvent::new(run, None, kind, payload))
                    .await
                    .unwrap();
                tx.commit().await.unwrap();
            }
        };
        append(
            OUTBOX_DELIVERED,
            json!({ "target": "github.open_pr", "repo": "acme/app",
                    "detail": { "number": 9, "url": "https://x/9" } }),
        )
        .await;
        let status = || async { engine.task_repos(task_id).await.unwrap()[0].status };
        assert_eq!(status().await, "open");

        let state = |branch: String, state: &str| ModEvent {
            mod_name: "github".into(),
            topic: crate::triggers::PR_STATE_TRIGGER.into(),
            payload: json!({ "repo": "acme/app", "number": 9, "branch": branch, "state": state }),
        };
        let ours = crate::gates::tests::branch_of(&store, task_id).await;
        assert!(
            engine
                .record_pr_state(&state(ours.clone(), "closed"))
                .await
                .unwrap()
        );
        assert_eq!(status().await, "closed");
        assert!(
            engine
                .record_pr_state(&state(ours.clone(), "open"))
                .await
                .unwrap()
        );
        assert_eq!(status().await, "open");
        // Not Sun's branch; a bad state.
        assert!(
            !engine
                .record_pr_state(&state("feat/x".into(), "closed"))
                .await
                .unwrap()
        );
        assert!(
            engine
                .record_pr_state(&state(ours.clone(), "draft"))
                .await
                .is_err()
        );

        append(
            crate::quality::MERGED_TRIGGER,
            json!({ "repo": "acme/app", "number": 9 }),
        )
        .await;
        assert_eq!(status().await, "merged");
        engine
            .record_pr_state(&state(ours, "closed"))
            .await
            .unwrap();
        assert_eq!(status().await, "merged");
    }

    /// `merge_order` fails on a PR closed without merging; after a reopen a
    /// retry waits again, and the merge passes it.
    #[tokio::test(flavor = "multi_thread")]
    async fn merge_order_fails_on_a_closed_pr_and_waits_again_after_reopen() {
        use std::time::Duration;

        use sun_core::{HostId, ModEvent, RunStatus};
        use sun_store::{NewEvent, Store};

        use crate::gates::tests::{LocalHost, events, mods, seed_with, until_run_ends};

        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).await.unwrap();
        let config = crate::EngineConfig {
            host: HostId::generate(),
            adapter: "fake".into(),
            delivery_timeout: Duration::from_secs(5),
            watchdog: crate::WatchdogConfig::default(),
        };
        let host = LocalHost::new(dir.path(), |_, _| {});
        let ci = mods(json!({ "state": "success", "checks": [] }));
        let (engine, _task) = Engine::start(store.clone(), host, ci, config)
            .await
            .unwrap();
        let api = repo("https://github.com/madlabs/api.git");
        let mut tx = store.begin().await.unwrap();
        tx.insert_repo(&api).await.unwrap();
        tx.commit().await.unwrap();
        let yaml = "name: q\nsteps:\n  - id: merged\n    type: gate\n    gates: [merge_order]\n";
        let run = seed_with(&store, yaml, vec![api.id]).await;
        let task_id = engine.run(run).await.unwrap().task_id;
        let append = |kind: &'static str, payload: Value| {
            let store = store.clone();
            async move {
                let mut tx = store.begin().await.unwrap();
                tx.append_event(NewEvent::new(run, None, kind, payload))
                    .await
                    .unwrap();
                tx.commit().await.unwrap();
            }
        };
        append(
            OUTBOX_DELIVERED,
            json!({ "target": "github.open_pr", "repo": "madlabs/api",
                    "detail": { "number": 12, "url": "https://x/12", "head_sha": "abc1def" } }),
        )
        .await;
        let waits = || async {
            events(&store, run, "gate_result")
                .await
                .iter()
                .filter(|e| e["result"]["waiting"] == true)
                .count()
        };
        let until_waits = |n: usize| async move {
            for _ in 0..1000 {
                if waits().await >= n {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            panic!("the gate never waited {n} time(s)");
        };
        let branch = crate::gates::tests::branch_of(&store, task_id).await;
        let state = |state: &str| ModEvent {
            mod_name: "github".into(),
            topic: crate::triggers::PR_STATE_TRIGGER.into(),
            payload: json!({ "repo": "madlabs/api", "number": 12,
                             "branch": branch, "state": state }),
        };

        engine.start_run(run).await.unwrap();
        until_waits(1).await;
        // Closed without merging: the gate fails at once, the run with it.
        engine.record_pr_state(&state("closed")).await.unwrap();
        assert_eq!(until_run_ends(&store, run).await.status, RunStatus::Failed);
        let results = events(&store, run, "gate_result").await;
        let last = &results.last().unwrap()["result"];
        assert_eq!(last["pass"], false, "{results:#?}");
        assert_eq!(
            last["output"],
            "PR #12 in madlabs/api was closed without merging. Reopen and merge it, then retry."
        );

        // Reopened and retried: waiting for the merge again; the merge passes it.
        engine.record_pr_state(&state("open")).await.unwrap();
        let step = StepKey::new("merged").unwrap();
        engine.retry_step(run, step, "ana".into()).await.unwrap();
        until_waits(2).await;
        append(
            crate::quality::MERGED_TRIGGER,
            json!({ "repo": "madlabs/api", "number": 12 }),
        )
        .await;
        engine.recheck(run).await;
        assert_eq!(
            until_run_ends(&store, run).await.status,
            RunStatus::Succeeded
        );
    }

    #[test]
    fn multi_repo_merge_order_waits_passes_and_catches_out_of_order() {
        let check = |merged: [Option<i64>; 3]| {
            let prs: Vec<RepoPr> = ["a/api", "a/web", "a/app"]
                .iter()
                .zip(merged)
                .map(|(s, m)| pr(s, m))
                .collect();
            merge_order(&prs.iter().collect::<Vec<_>>())
        };
        assert_eq!(check([None, None, None]), Order::Waiting { next: 0 });
        assert_eq!(check([Some(5), None, None]), Order::Waiting { next: 1 });
        assert_eq!(check([Some(5), Some(9), Some(12)]), Order::Done);
        assert!(
            matches!(check([None, Some(5), None]), Order::Broken(why) if why.starts_with("a/web merged before a/api"))
        );
        assert!(matches!(
            check([Some(9), Some(5), Some(12)]),
            Order::Broken(_)
        ));
    }
}
