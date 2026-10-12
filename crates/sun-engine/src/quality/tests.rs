//! Post-merge quality end to end: a real origin repo with a squash merge, a
//! fix commit and a revert; the host fake runs git in the task worktree.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::Path;
use std::process::Command;
use std::time::Duration;

use serde_json::json;
use sun_core::HostId;
use sun_store::{RunRepo, Store};

use super::*;
use crate::EngineConfig;
use crate::gates::tests::{LocalHost, events, mods, seed};

fn git(cwd: &Path, args: &str) -> String {
    let out = Command::new("sh")
        .arg("-c")
        .arg(format!("git -c user.email=t@t -c user.name=t {args}"))
        .current_dir(cwd)
        .output()
        .unwrap();
    assert!(out.status.success(), "git {args}: {out:?}");
    String::from_utf8(out.stdout).unwrap().trim().to_owned()
}

const WORKFLOW: &str = "name: q\nsteps:\n  - id: a\n    type: agent\n    prompt: x\n";

#[tokio::test(flavor = "multi_thread")]
async fn quality_tracks_ci_revert_and_fix_commits_after_merge() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    // origin with a base commit; the task worktree is a clone of it.
    git(root, "init -q --bare -b main origin.git");
    git(root, "clone -q origin.git seed");
    let seed_dir = root.join("seed");
    std::fs::write(seed_dir.join("app.rs"), "v1").unwrap();
    std::fs::write(seed_dir.join("other.rs"), "v1").unwrap();
    git(
        &seed_dir,
        "add -A && git -c user.email=t@t -c user.name=t commit -qm base",
    );
    git(&seed_dir, "push -q origin HEAD:main");
    std::fs::create_dir_all(root.join("work")).unwrap();
    git(root, "clone -q origin.git work/task");
    let merged_at = Utc::now() - chrono::Duration::hours(1);

    // After the clone: the squash merge, then follow-ups on main.
    std::fs::write(seed_dir.join("app.rs"), "v2").unwrap();
    git(&seed_dir, "commit -qam 'feat: add x (#9)'");
    let merge_sha = git(&seed_dir, "rev-parse HEAD");
    std::fs::write(seed_dir.join("app.rs"), "v3").unwrap();
    git(&seed_dir, "commit -qam 'fix: crash in x'");
    let fix_sha = git(&seed_dir, "rev-parse HEAD");
    std::fs::write(seed_dir.join("other.rs"), "v2").unwrap();
    git(&seed_dir, "commit -qam 'fix: unrelated file'");
    std::fs::write(seed_dir.join("app.rs"), "v4").unwrap();
    git(&seed_dir, "commit -qam 'chore: tidy'");
    // What `git revert` writes (by hand: the tidy commit would conflict).
    std::fs::write(seed_dir.join("app.rs"), "v1").unwrap();
    let message = format!("-m 'Revert \"feat: add x (#9)\"' -m 'This reverts commit {merge_sha}.'");
    git(&seed_dir, &format!("commit -qa {message}"));
    let revert_sha = git(&seed_dir, "rev-parse HEAD");
    git(&seed_dir, "push -q origin HEAD:main");

    let store = Store::open(root).await.unwrap();
    let host = LocalHost::new(root, |_, _| {});
    let ci = mods(json!({ "state": "success", "checks": [] }));
    let config = EngineConfig {
        host: HostId::generate(),
        adapter: "fake".into(),
        delivery_timeout: Duration::from_secs(5),
        watchdog: crate::WatchdogConfig::default(),
    };
    let (engine, _task) = Engine::start(store.clone(), host, ci.clone(), config)
        .await
        .unwrap();
    let run = seed(&store, WORKFLOW).await;
    let task_id = store
        .read()
        .await
        .unwrap()
        .get_run(run)
        .await
        .unwrap()
        .task_id;
    let merge = |branch: String, sha: &str| Merge {
        repo: "acme/app".into(),
        number: Some(9),
        branch,
        base: "main".into(),
        merge_sha: sha.into(),
        merged_at,
    };

    // Branches Sun didn't make are not ours; bad input never reaches git.
    let other = engine.record_merge("github", merge("feat/x".into(), &merge_sha));
    assert_eq!(other.await.unwrap(), None);
    let unknown = engine.record_merge("github", merge("sun/MAIN-999-x".into(), &merge_sha));
    assert_eq!(unknown.await.unwrap(), None, "a key no task has");
    let branch = crate::gates::tests::branch_of(&store, task_id).await;
    let bad = merge(branch.clone(), "abc; rm -rf /");
    assert!(engine.record_merge("github", bad).await.is_err());
    assert!(engine.check_quality(task_id).await.is_err(), "not merged");

    let recorded = engine
        .record_merge("github", merge(branch, &merge_sha))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(recorded.held_up(), None);
    let checked = engine.check_quality(task_id).await.unwrap();

    assert_eq!(checked.ci_state.as_deref(), Some("success"));
    assert_eq!(checked.reverted_by.as_deref(), Some(revert_sha.as_str()));
    let fixes: Vec<(&str, &str)> = checked
        .fix_commits
        .iter()
        .map(|c| (c.sha.as_str(), c.subject.as_str()))
        .collect();
    assert_eq!(fixes, [(fix_sha.as_str(), "fix: crash in x")]);
    assert_eq!(checked.held_up(), Some(false));
    let call = ci.calls.lock().unwrap()[0].clone();
    assert_eq!(
        (call.mod_name.as_str(), call.method.as_str()),
        ("github", "ci_status")
    );
    assert_eq!(call.params, json!({ "repo": "acme/app", "ref": merge_sha }));

    assert_eq!(engine.quality(task_id).await.unwrap(), Some(checked));
    let metrics = engine.quality_metrics().await.unwrap();
    assert_eq!(
        (
            metrics.merged,
            metrics.decided,
            metrics.reverted,
            metrics.with_fixes
        ),
        (1, 1, 1, 1)
    );
    assert_eq!(metrics.held_up_rate, Some(0.0));
    assert_eq!(events(&store, run, "pr_merged").await.len(), 1);
    let check = &events(&store, run, "quality_checked").await[0];
    assert_eq!(check["held_up"], false);
}

/// The issue that started the task is linked at the merge; a bug
/// issue opened after it that names the merged code is a regression.
#[tokio::test(flavor = "multi_thread")]
async fn quality_links_bug_issues_and_records_regressions() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    git(root, "init -q --bare -b main origin.git");
    git(root, "clone -q origin.git seed");
    let seed_dir = root.join("seed");
    std::fs::write(seed_dir.join("app.rs"), "v1").unwrap();
    git(
        &seed_dir,
        "add -A && git -c user.email=t@t -c user.name=t commit -qm base",
    );
    std::fs::write(seed_dir.join("app.rs"), "v2").unwrap();
    git(&seed_dir, "commit -qam 'fix: login 500 (#9)'");
    let merge_sha = git(&seed_dir, "rev-parse HEAD");
    git(&seed_dir, "push -q origin HEAD:main");
    std::fs::create_dir_all(root.join("work")).unwrap();
    git(root, "clone -q origin.git work/task");
    let merged_at = Utc::now() - chrono::Duration::days(2);
    let later = (merged_at + chrono::Duration::days(1)).to_rfc3339();
    let earlier = (merged_at - chrono::Duration::days(1)).to_rfc3339();

    let store = Store::open(root).await.unwrap();
    let host = LocalHost::new(root, |_, _| {});
    let origin_url = "https://github.com/acme/app/issues/4";
    let issue = |key: &str, title: &str, labels: Value, at: &str| {
        json!({ "key": key, "title": title, "url": format!("https://github.com/{}", key.replace('#', "/issues/")),
                "labels": labels, "created_at": at })
    };
    let reply = json!({
        "state": "success",
        "issues": [
            issue("acme/app#20", "Login loops after the 500 fix", json!(["bug"]), &later),
            issue("acme/app#21", "Add dark mode to login", json!(["enhancement"]), &later),
            issue("acme/app#3", "Login is slow", json!(["bug"]), &earlier),
            issue("acme/app#4", "Login 500s", json!(["bug"]), &later),
        ],
    });
    let plugin = mods(reply);
    let config = EngineConfig {
        host: HostId::generate(),
        adapter: "fake".into(),
        delivery_timeout: Duration::from_secs(5),
        watchdog: crate::WatchdogConfig::default(),
    };
    let (engine, _task) = Engine::start(store.clone(), host, plugin.clone(), config)
        .await
        .unwrap();
    let run = seed(&store, WORKFLOW).await;
    let mut tx = store.begin().await.unwrap();
    let task_id = tx.get_run(run).await.unwrap().task_id;
    // The task came from `github.issue_labeled` on issue #4.
    let trigger =
        json!({ "repo": "acme/app", "number": 4, "title": "Login 500s", "url": origin_url });
    let key = format!("{}{task_id}", crate::triggers::TRIGGER_OF);
    tx.put_setting(&key, &trigger).await.unwrap();
    tx.commit().await.unwrap();

    let merge = Merge {
        repo: "acme/app".into(),
        number: Some(9),
        branch: crate::gates::tests::branch_of(&store, task_id).await,
        base: "main".into(),
        merge_sha: merge_sha.clone(),
        merged_at,
    };
    let recorded = engine.record_merge("github", merge).await.unwrap().unwrap();
    let origin = BugIssue {
        relation: BugRelation::Origin,
        key: "acme/app#4".into(),
        title: Some("Login 500s".into()),
        url: Some(origin_url.into()),
        opened_at: None,
    };
    assert_eq!(recorded.bug_issues, std::slice::from_ref(&origin));
    assert_eq!(
        events(&store, run, "pr_merged").await[0]["bug_issues"],
        json!(["acme/app#4"])
    );

    let checked = engine.check_quality(task_id).await.unwrap();
    // Only the bug opened after the merge; not the feature, the older bug or
    // the origin issue itself.
    let regression = BugIssue {
        relation: BugRelation::Regression,
        key: "acme/app#20".into(),
        title: Some("Login loops after the 500 fix".into()),
        url: Some("https://github.com/acme/app/issues/20".into()),
        opened_at: Some(chrono::DateTime::parse_from_rfc3339(&later).unwrap().into()),
    };
    assert_eq!(checked.bug_issues, [origin, regression]);
    assert_eq!(checked.held_up(), Some(false), "green CI, but a regression");
    let search = plugin.calls.lock().unwrap()[1].clone();
    assert_eq!(
        (search.mod_name.as_str(), search.method.as_str()),
        ("github", "search_issues")
    );
    assert_eq!(search.params["terms"], json!([&merge_sha[..7], "app.rs"]));
    assert_eq!(search.params["repo"], "acme/app");
    assert_eq!(engine.quality_metrics().await.unwrap().with_bugs, 1);
    assert_eq!(
        events(&store, run, "quality_checked").await[0]["regressions"],
        1
    );
}

#[test]
fn quality_links_issues_from_the_task_text() {
    let task = Task {
        body: "See https://acme.atlassian.net/browse/APP-7, \
               (https://gitlab.com/g/sub/p/-/issues/3) and https://github.com/acme/app/pull/9 \
               and #9 and https://github.com/acme/app/issues/12#issuecomment-1."
            .into(),
        workflow_id: Some(sun_core::WorkflowId::generate()),
        status: sun_core::TaskStatus::Backlog,
        ..Task::new(sun_core::WorkspaceId::MAIN, "Fix #12 and APP-7")
    };
    let keys: Vec<(BugRelation, String, Option<String>)> =
        linked_issues(&task, None, "acme/app", Some(9))
            .into_iter()
            .map(|b| (b.relation, b.key, b.url))
            .collect();
    let referenced = BugRelation::Referenced;
    assert_eq!(
        keys,
        [
            (referenced, "acme/app#12".into(), None),
            (
                referenced,
                "APP-7".into(),
                Some("https://acme.atlassian.net/browse/APP-7".into())
            ),
            (
                referenced,
                "g/sub/p#3".into(),
                Some("https://gitlab.com/g/sub/p/-/issues/3".into())
            ),
        ],
        "the PR itself (#9, /pull/9) is not an issue; each key once"
    );
    assert_eq!(issue_key("javascript://x/a/b/issues/1"), None);
    assert!(is_bug(&["Type: Bug".into()], "Dark mode"));
    assert!(!is_bug(&["enhancement".into()], "Dark mode"));
}

#[test]
fn quality_is_fix_reads_commit_subjects() {
    assert!(is_fix("fix: crash"));
    assert!(is_fix("Hotfix for login"));
    assert!(is_fix("fix(api): 500 on empty body"));
    assert!(!is_fix("feat: prefix support"));
    assert!(!is_fix("chore: tidy"));
}
