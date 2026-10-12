//! The learning loop end to end: proposals from repeats and stars, the
//! repo-scope PR through the outbox, approval, P3 delivery and the metric.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::Path;
use std::time::{Duration, Instant};

use serde_json::json;
use sun_core::model::{Correction, CorrectionKind, Repo, Run};
use sun_core::{HostId, HostLink, RunId, StepKey};
use sun_store::{Store, TaskContext, TaskRepo};

use super::*;
use crate::EngineConfig;
use crate::gates::tests::{LocalHost, mods, seed};

const FLOW: &str = "name: fix\nsteps:\n  - id: implement\n    type: agent\n    prompt: fix it\n";

fn correction(task: &Task, run: RunId, cause: CorrectionCause, body: &str) -> Correction {
    Correction {
        id: CorrectionId::generate(),
        task_id: task.id,
        run_id: run,
        workflow_id: task.workflow_id.unwrap(),
        step_key: Some(StepKey::new("implement").unwrap()),
        runtime: Some("claude-code".into()),
        kind: CorrectionKind::GateFail,
        cause_label: cause,
        classified_by: "rules".into(),
        body: body.into(),
        created_at: Utc::now(),
    }
}

/// Records `c` the way the engine's capture hooks do.
async fn record(store: &Store, c: &Correction) {
    let mut tx = store.begin().await.unwrap();
    assert!(tx.insert_correction(c, None).await.unwrap());
    on_correction(&mut tx, c).await.unwrap();
    tx.commit().await.unwrap();
}

async fn task_of(store: &Store, run: RunId) -> Task {
    let mut tx = store.read().await.unwrap();
    let run = tx.get_run(run).await.unwrap();
    tx.get_task(run.task_id).await.unwrap()
}

fn git(cwd: &Path, args: &str) -> String {
    let out = std::process::Command::new("sh")
        .arg("-c")
        .arg(format!("git {args}"))
        .current_dir(cwd)
        .output()
        .unwrap();
    assert!(out.status.success(), "git {args}: {out:?}");
    String::from_utf8_lossy(&out.stdout).trim().to_owned()
}

#[tokio::test]
async fn memory_repeated_cause_proposes_once_and_waits_for_a_human() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).await.unwrap();
    let run = seed(&store, FLOW).await;
    let task = task_of(&store, run).await;
    let lint = correction(&task, run, CorrectionCause::Lint, "clippy: unused import");
    record(&store, &lint).await;
    for n in 0..2 {
        let body = format!("Gate `test` failed: assertion {n}");
        record(
            &store,
            &correction(&task, run, CorrectionCause::TestFailure, &body),
        )
        .await;
    }
    let items = || async { store.read().await.unwrap().memory_items().await.unwrap() };
    assert!(
        items().await.is_empty(),
        "two of a cause are not a pattern yet"
    );

    let third = correction(&task, run, CorrectionCause::TestFailure, "tests fail\nmore");
    record(&store, &third).await;
    record(&store, &third_like(&third)).await;
    let items = items().await;
    assert_eq!(items.len(), 1, "one proposal per workflow and cause");
    let (item, pr) = &items[0];
    assert_eq!(item.status, MemoryStatus::Proposed);
    assert_eq!(
        item.scope,
        MemoryScope::Workflow,
        "no repo: an approval card"
    );
    assert_eq!(item.scope_id, task.workflow_id.map(|w| w.to_string()));
    assert_eq!((item.author.as_str(), item.source_run), ("sun", Some(run)));
    assert!(item.body.starts_with("Run the tests"), "{}", item.body);
    assert!(
        item.reason
            .starts_with("test_failure corrected 3 times in fix; latest: tests fail"),
        "{}",
        item.reason
    );
    assert!(pr.is_none());

    // Nothing reaches a pack until a human approves it.
    let mut tx = store.read().await.unwrap();
    assert!(active_for(&mut tx, &task).await.unwrap().is_empty());
    drop(tx);
    let mut tx = store.begin().await.unwrap();
    let retire_first = tx
        .review_memory(item.id, MemoryStatus::Active, "user:ana")
        .await;
    assert_eq!(
        retire_first.unwrap().reviewed_by.as_deref(),
        Some("user:ana")
    );
    let again = tx
        .review_memory(item.id, MemoryStatus::Active, "user:ana")
        .await;
    assert!(again.is_err(), "active → active is no review move");
    tx.commit().await.unwrap();
    let mut tx = store.read().await.unwrap();
    assert_eq!(active_for(&mut tx, &task).await.unwrap().len(), 1);
}

#[tokio::test]
async fn memory_relabel_counts_and_a_rejected_cause_returns_when_it_keeps_recurring() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).await.unwrap();
    let run = seed(&store, FLOW).await;
    let task = task_of(&store, run).await;
    let proposals = || async {
        let items = store.read().await.unwrap().memory_items().await.unwrap();
        items.into_iter().map(|(m, _)| m).collect::<Vec<_>>()
    };
    for _ in 0..2 {
        let c = correction(&task, run, CorrectionCause::Lint, "clippy");
        record(&store, &c).await;
    }
    // A classifier relabels an `other` correction as lint: the third counts.
    let other = correction(&task, run, CorrectionCause::Other, "warnings everywhere");
    record(&store, &other).await;
    assert!(proposals().await.is_empty());
    let mut tx = store.begin().await.unwrap();
    relabel(&mut tx, other.id, CorrectionCause::Lint, "model")
        .await
        .unwrap();
    tx.commit().await.unwrap();
    let items = proposals().await;
    assert_eq!(items.len(), 1);
    assert!(items[0].body.starts_with("Run the linters"));

    // Rejected: no new proposal until lint is corrected 3 more times.
    let mut tx = store.begin().await.unwrap();
    tx.review_memory(items[0].id, MemoryStatus::Retired, "user:ana")
        .await
        .unwrap();
    tx.commit().await.unwrap();
    for n in 0..3 {
        assert_eq!(proposals().await.len(), 1, "after {n} more");
        let c = correction(&task, run, CorrectionCause::Lint, "clippy again");
        record(&store, &c).await;
    }
    let items = proposals().await;
    assert_eq!(items.len(), 2, "it kept recurring: proposed again");
    assert_eq!(items[0].status, MemoryStatus::Proposed);
    // While that one is open, more repeats propose nothing.
    for _ in 0..3 {
        let c = correction(&task, run, CorrectionCause::Lint, "clippy");
        record(&store, &c).await;
    }
    assert_eq!(proposals().await.len(), 2);
}

fn third_like(c: &Correction) -> Correction {
    Correction {
        id: CorrectionId::generate(),
        ..c.clone()
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn memory_starred_correction_opens_a_pr_and_reaches_packs_once_approved() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).await.unwrap();
    let host = LocalHost::new(dir.path(), |_, _| {});
    let pr_url = "https://github.com/acme/web/pull/7";
    let mods = mods(json!({ "number": 7, "url": pr_url, "created": true }));
    let config = EngineConfig {
        host: HostId::generate(),
        adapter: "fake".into(),
        delivery_timeout: Duration::from_secs(5),
        watchdog: crate::WatchdogConfig::default(),
    };
    let (engine, _task) = Engine::start(store.clone(), host.clone(), mods.clone(), config)
        .await
        .unwrap();

    // A task on one repo, with a correction a human stars.
    let seeded = seed(&store, FLOW).await;
    let mut task = task_of(&store, seeded).await;
    let repo = Repo {
        id: RepoId::generate(),
        workspace_id: sun_core::WorkspaceId::MAIN,
        url: "https://github.com/acme/web.git".into(),
        default_branch: "main".into(),
        gate_config: json!({}),
        description: String::new(),
        hook_id: None,
    };
    task.id = TaskId::generate();
    let run = Run {
        id: RunId::generate(),
        task_id: task.id,
        workflow_id: task.workflow_id.unwrap(),
        workflow_version: 1,
        status: sun_core::RunStatus::Queued,
        end_reason: None,
        budget: Default::default(),
        repo_ids: vec![repo.id],
    };
    let mut tx = store.begin().await.unwrap();
    tx.insert_repo(&repo).await.unwrap();
    tx.insert_task(&task).await.unwrap();
    tx.insert_run(&run).await.unwrap();
    let starred = correction(
        &task,
        run.id,
        CorrectionCause::Style,
        "Use snake_case for `userId`",
    );
    assert!(tx.insert_correction(&starred, None).await.unwrap());
    tx.commit().await.unwrap();
    assert!(
        engine
            .star_correction(starred.id, "user:ana")
            .await
            .unwrap()
    );
    assert!(
        !engine
            .star_correction(starred.id, "user:ana")
            .await
            .unwrap(),
        "once"
    );

    let view = engine.memory().await.unwrap();
    let item = view[0].item.clone();
    assert_eq!(
        (item.scope, view[0].scope_name.as_deref()),
        (MemoryScope::Repo, Some("acme/web"))
    );
    assert_eq!(item.author, "user:ana");
    assert_eq!(
        item.body,
        "Don't repeat this past mistake: Use snake_case for `userId`"
    );

    // The host's worktree pushes to a bare remote.
    let worktree = Worktree::Folder { task_id: task.id };
    let cwd = host
        .prepare_worktree(HostId::generate(), worktree)
        .await
        .unwrap();
    git(dir.path(), "init -q --bare remote.git");
    git(
        &cwd,
        &format!(
            "remote add origin {}",
            dir.path().join("remote.git").display()
        ),
    );

    assert_eq!(engine.open_memory_prs().await.unwrap(), 1);
    assert_eq!(engine.open_memory_prs().await.unwrap(), 0, "enqueued once");
    let branch = format!("sun/{}", item.id);
    let agents = git(
        dir.path(),
        &format!("--git-dir remote.git show {branch}:AGENTS.md"),
    );
    assert_eq!(
        agents,
        "## Learned from corrections\n\n- Don't repeat this past mistake: Use snake_case for `userId`"
    );

    // The git host mod's `open_pr`, delivered through the outbox.
    let deadline = Instant::now() + Duration::from_secs(10);
    let pr = loop {
        if let Some(pr) = engine.memory().await.unwrap()[0].pr.clone()
            && pr.status == OutboxStatus::Delivered
        {
            break pr;
        }
        assert!(Instant::now() < deadline, "PR never delivered");
        tokio::time::sleep(Duration::from_millis(20)).await;
    };
    assert_eq!(pr.url.as_deref(), Some(pr_url));
    let call = mods.calls.lock().unwrap()[0].clone();
    assert_eq!(
        (call.mod_name.as_str(), call.method.as_str()),
        ("github", "open_pr")
    );
    assert_eq!(
        (
            &call.params["repo"],
            &call.params["head"],
            &call.params["base"]
        ),
        (&json!("acme/web"), &json!(branch), &json!("main"))
    );

    // Still only proposed: not in the pack until approved, then in it as P3.
    let context = TaskContext::default();
    let pack = engine
        .preview_context(task.id, &context)
        .await
        .unwrap()
        .pack;
    assert!(!pack.render_markdown().contains("snake_case"));
    engine
        .review_memory(item.id, MemoryStatus::Active, "user:ben")
        .await
        .unwrap();
    let pack = engine
        .preview_context(task.id, &context)
        .await
        .unwrap()
        .pack;
    assert!(pack.render_markdown().contains("Memory (repo)"));
    assert!(
        pack.render_markdown()
            .contains("Use snake_case for `userId`")
    );
    engine
        .review_memory(item.id, MemoryStatus::Retired, "user:ben")
        .await
        .unwrap();
    let pack = engine
        .preview_context(task.id, &context)
        .await
        .unwrap()
        .pack;
    assert!(!pack.render_markdown().contains("snake_case"), "retired");
}

#[test]
fn memory_first_pass_gate_rate_counts_each_steps_first_attempt() {
    let now = Utc::now();
    let (a, b) = (RunId::generate(), RunId::generate());
    let result = |run, step: &str, attempt: u32, pass: bool, days: i64| EventRecord {
        seq: 0,
        run_id: run,
        step_key: Some(StepKey::new(step).unwrap()),
        kind: "gate_result".into(),
        payload: json!({ "attempt": attempt, "result": { "name": "test", "pass": pass } }),
        blob: None,
        at: now - chrono::Duration::days(days),
    };
    let events = [
        // Run a: `verify` failed first (passed on attempt 2); `lint` passed.
        result(a, "verify", 2, true, 1),
        result(a, "verify", 1, true, 2),
        result(a, "verify", 1, false, 2),
        result(a, "lint", 1, true, 2),
        // Run b, 3 weeks ago: passed first time.
        result(b, "verify", 1, true, 20),
    ];
    // Run a's checks checked codex's work, run b's claude's.
    let got = first_pass_metrics(
        &events,
        now,
        |run| if run == a { "fix" } else { "docs" }.to_owned(),
        |run, _| if run == a { "codex" } else { "claude" }.to_owned(),
    );
    assert_eq!((got.overall.gate_steps, got.overall.first_pass), (3, 2));
    let last = &got.weeks[7].rate;
    assert_eq!((last.gate_steps, last.first_pass), (2, 1));
    assert_eq!(got.weeks[5].rate.first_pass, 1);
    assert_eq!(got.by_workflow[0].name, "fix");
    assert_eq!(got.by_workflow[0].rate.rate, Some(0.5));
    // first_pass.by_runtime: codex 1 of 2, claude 1 of 1.
    let rate = |name: &str| {
        let r = got.by_runtime.iter().find(|r| r.name == name).unwrap();
        (r.rate.gate_steps, r.rate.first_pass)
    };
    assert_eq!((rate("codex"), rate("claude")), ((2, 1), (1, 1)));
}

/// Review rejections are the `reject` corrections only, counted
/// over time, per workflow and per runtime.
#[tokio::test]
async fn memory_metrics_count_review_rejections() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).await.unwrap();
    let host = LocalHost::new(dir.path(), |_, _| {});
    let config = EngineConfig {
        host: HostId::generate(),
        adapter: "fake".into(),
        delivery_timeout: Duration::from_secs(5),
        watchdog: crate::WatchdogConfig::default(),
    };
    let (engine, _task) = Engine::start(store.clone(), host, mods(json!({})), config)
        .await
        .unwrap();
    let run = seed(&store, FLOW).await;
    let task = task_of(&store, run).await;
    for kind in [
        CorrectionKind::Reject,
        CorrectionKind::Reject,
        CorrectionKind::GateFail,
        CorrectionKind::ReviewComment,
    ] {
        let c = Correction {
            kind,
            ..correction(&task, run, CorrectionCause::Other, "no")
        };
        record(&store, &c).await;
    }
    let got = engine.memory_metrics().await.unwrap().review_rejections;
    assert_eq!((got.overall.corrections, got.overall.repeats), (2, 1));
    assert_eq!(got.weeks[7].rate.corrections, 2);
    assert_eq!(
        (
            got.by_workflow[0].name.as_str(),
            got.by_workflow[0].rate.corrections
        ),
        ("fix", 2)
    );
    assert_eq!(got.by_runtime[0].name, "claude-code");
}

#[test]
fn memory_repeat_rate_by_week_workflow_and_runtime() {
    let now = Utc::now();
    let (fix, docs) = (WorkflowId::generate(), WorkflowId::generate());
    let at = |days: i64, workflow, cause, runtime: Option<&str>| Correction {
        id: CorrectionId::generate(),
        task_id: TaskId::generate(),
        run_id: RunId::generate(),
        workflow_id: workflow,
        step_key: None,
        runtime: runtime.map(str::to_owned),
        kind: CorrectionKind::GateFail,
        cause_label: cause,
        classified_by: "rules".into(),
        body: String::new(),
        created_at: now - chrono::Duration::days(days),
    };
    use CorrectionCause::*;
    let rows = [
        at(20, fix, TestFailure, Some("claude")),
        at(15, fix, TestFailure, Some("claude")), // repeat
        at(1, fix, TestFailure, Some("codex")),   // repeat
        at(1, docs, TestFailure, None),           // other workflow: new
        at(0, fix, Lint, Some("codex")),          // other cause: new
        at(100, docs, Style, None),               // older than 8 weeks
    ];
    let name = |id| {
        if id == fix {
            "fix".into()
        } else {
            "docs".into()
        }
    };
    let (overall, weeks, by_workflow, by_runtime) = repeat_metrics(&rows, now, name);
    assert_eq!((overall.corrections, overall.repeats), (6, 2));
    assert_eq!(overall.rate, Some(2.0 / 6.0));
    assert_eq!(weeks.len(), 8);
    assert!(weeks[0].week_start < weeks[7].week_start, "oldest first");
    let last = &weeks[7].rate;
    assert_eq!((last.corrections, last.repeats), (3, 1));
    assert_eq!((weeks[5].rate.corrections, weeks[5].rate.repeats), (2, 1));
    assert_eq!((weeks[4].rate.corrections, weeks[4].rate.repeats), (0, 0));
    assert_eq!(weeks[4].rate.rate, None);
    assert_eq!(by_workflow[0].name, "fix");
    assert_eq!(
        (by_workflow[0].rate.corrections, by_workflow[0].rate.repeats),
        (4, 2)
    );
    let codex = by_runtime.iter().find(|r| r.name == "codex").unwrap();
    assert_eq!((codex.rate.corrections, codex.rate.repeats), (2, 1));
    assert!(by_runtime.iter().any(|r| r.name == "none"));
}

#[test]
fn memory_edit_script_quotes_the_branch_and_reads_the_line_from_env() {
    let script = edit_script("sun/01J");
    assert!(script.contains("\"$SUN_MEMORY_LINE\""));
    assert!(script.contains("## Learned from corrections"));
}

/// The script edits the first candidate the repo has, else `AGENTS.md`, or
/// `CLAUDE.md` when the repo has only that; it names the file it edited.
#[test]
fn memory_edit_script_picks_the_file_the_repo_has() {
    let edit = |files: &[&str], target: Option<&str>| {
        let dir = tempfile::tempdir().unwrap();
        git(dir.path(), "init -q");
        for f in files {
            let path = dir.path().join(f);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, "# x\n").unwrap();
        }
        let out = std::process::Command::new("sh")
            .arg("-c")
            .arg(edit_script("sun/01J"))
            .current_dir(dir.path())
            .env("SUN_MEMORY_FILES", candidates(target).join(" "))
            .env("SUN_MEMORY_LINE", "Run the tests.")
            .output()
            .unwrap();
        let out = String::from_utf8_lossy(&out.stdout).into_owned();
        let file = out.trim().strip_prefix(EDITED).unwrap().to_owned();
        let text = std::fs::read_to_string(dir.path().join(&file)).unwrap();
        assert!(text.ends_with("- Run the tests.\n"), "{text}");
        file
    };
    assert_eq!(edit(&[], None), "AGENTS.md");
    assert_eq!(edit(&["CLAUDE.md"], None), "CLAUDE.md");
    assert_eq!(edit(&["CLAUDE.md", "AGENTS.md"], None), "AGENTS.md");
    let skill = Some(".claude/skills/tdd/SKILL.md");
    assert_eq!(
        edit(&[".agents/skills/tdd/SKILL.md"], skill),
        ".agents/skills/tdd/SKILL.md",
        "any runtime's skills folder"
    );
    assert_eq!(edit(&[], skill), "AGENTS.md", "a skill the repo lacks");
    assert!(candidates(Some("../etc/passwd")).is_empty());
}

/// A workflow-scope proposal (a multi-repo task) is opened as a PR too, in
/// the task's first repo, editing the corrected step's prompt file.
#[tokio::test(flavor = "multi_thread")]
async fn workflow_scope_proposal_opens_a_pr() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).await.unwrap();
    let host = LocalHost::new(dir.path(), |_, _| {});
    let mods =
        mods(json!({ "number": 8, "url": "https://gitlab.com/acme/api/-/merge_requests/8" }));
    let config = EngineConfig {
        host: HostId::generate(),
        adapter: "fake".into(),
        delivery_timeout: Duration::from_secs(5),
        watchdog: crate::WatchdogConfig::default(),
    };
    let (engine, _task) = Engine::start(store.clone(), host.clone(), mods.clone(), config)
        .await
        .unwrap();

    let repo = |url: &str| Repo {
        id: RepoId::generate(),
        workspace_id: sun_core::WorkspaceId::MAIN,
        url: url.into(),
        default_branch: "main".into(),
        gate_config: json!({}),
        description: String::new(),
        hook_id: None,
    };
    let (api, web) = (
        repo("https://gitlab.com/acme/api.git"),
        repo("https://gitlab.com/acme/web.git"),
    );
    let mut tx = store.begin().await.unwrap();
    tx.insert_repo(&api).await.unwrap();
    tx.insert_repo(&web).await.unwrap();
    tx.commit().await.unwrap();
    let run = crate::gates::tests::seed_with(&store, FLOW, vec![api.id, web.id]).await;
    let task = task_of(&store, run).await;
    // The run's workflow came from the repo, its prompt from a file.
    let mut tx = store.begin().await.unwrap();
    let source = json!({ "path": ".sun/workflows/fix.yaml", "version": 1,
        "prompt_files": [".sun/prompts/fix.md"], "prompts": { "implement": ".sun/prompts/fix.md" },
        "missing": [] });
    tx.append_event(sun_store::NewEvent::new(
        run,
        None,
        crate::executor::repo_source::WORKFLOW_SOURCE,
        source,
    ))
    .await
    .unwrap();
    tx.commit().await.unwrap();
    for n in 0..3 {
        let body = format!("Gate `lint` failed: warning {n}");
        record(
            &store,
            &correction(&task, run, CorrectionCause::Lint, &body),
        )
        .await;
    }
    let item = engine.memory().await.unwrap()[0].item.clone();
    assert_eq!(item.scope, MemoryScope::Workflow);
    assert_eq!(item.target.as_deref(), Some(".sun/prompts/fix.md"));

    // The host's worktree has the prompt file and pushes to a bare remote.
    let cwd = host
        .prepare_worktree(HostId::generate(), Worktree::Folder { task_id: task.id })
        .await
        .unwrap();
    std::fs::create_dir_all(cwd.join(".sun/prompts")).unwrap();
    std::fs::write(cwd.join(".sun/prompts/fix.md"), "Fix the bug.\n").unwrap();
    git(dir.path(), "init -q --bare remote.git");
    let remote = dir.path().join("remote.git");
    git(&cwd, &format!("remote add origin {}", remote.display()));

    assert_eq!(engine.open_memory_prs().await.unwrap(), 1);
    assert_eq!(engine.open_memory_prs().await.unwrap(), 0, "enqueued once");
    let branch = format!("sun/{}", item.id);
    let prompt = git(
        dir.path(),
        &format!("--git-dir remote.git show {branch}:.sun/prompts/fix.md"),
    );
    assert_eq!(
        prompt,
        "Fix the bug.\n\n## Learned from corrections\n\n\
         - Run the linters and type checks and fix every warning before you finish."
    );
    let deadline = Instant::now() + Duration::from_secs(10);
    let view = loop {
        let view = engine.memory().await.unwrap().remove(0);
        if view
            .pr
            .as_ref()
            .is_some_and(|pr| pr.status == OutboxStatus::Delivered)
        {
            break view;
        }
        assert!(Instant::now() < deadline, "PR never delivered");
        tokio::time::sleep(Duration::from_millis(20)).await;
    };
    assert_eq!(view.item.target.as_deref(), Some(".sun/prompts/fix.md"));
    let call = mods.calls.lock().unwrap()[0].clone();
    assert_eq!(
        (call.mod_name.as_str(), call.method.as_str()),
        ("gitlab", "open_pr")
    );
    assert_eq!(call.params["repo"], "acme/api", "the task's first repo");
    assert!(
        call.params["body"]
            .as_str()
            .unwrap()
            .contains("to `.sun/prompts/fix.md`"),
        "{}",
        call.params["body"]
    );
}
