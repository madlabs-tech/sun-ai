//! A repo-scope skill install end to end: files in the native folders on
//! `sun/skill-<name>`, pushed to a bare remote, `open_pr` through the outbox,
//! idempotent per version.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::Path;
use std::time::{Duration, Instant};

use sun_core::{HostId, HostLink};
use sun_store::Store;

use super::*;
use crate::EngineConfig;
use crate::gates::tests::{LocalHost, mods};

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

async fn delivered(engine: &Engine, repo: RepoId) -> SkillPr {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let got = engine.skill_pr(repo, "tdd").await.unwrap().unwrap();
        if got.pr.as_ref().map(|p| p.status) == Some(OutboxStatus::Delivered) {
            return got;
        }
        assert!(Instant::now() < deadline, "PR never delivered: {got:?}");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[tokio::test]
async fn catalog_skill_repo_install_pushes_a_branch_and_opens_one_pr() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).await.unwrap();
    let host = LocalHost::new(dir.path(), |_, _| {});
    let pr_url = "https://github.com/acme/web/pull/9";
    let mods = mods(json!({ "number": 9, "url": pr_url, "created": true }));
    let config = EngineConfig {
        allow_bypass: false,
        host: HostId::generate(),
        adapter: "fake".into(),
        delivery_timeout: Duration::from_secs(5),
        watchdog: crate::WatchdogConfig::default(),
    };
    let (engine, _task) = Engine::start(store.clone(), host.clone(), mods.clone(), config)
        .await
        .unwrap();
    let repo = crate::gates::tests::repo("https://github.com/acme/web.git");
    let mut tx = store.begin().await.unwrap();
    tx.insert_repo(&repo).await.unwrap();
    tx.commit().await.unwrap();
    assert!(engine.skill_pr(repo.id, "tdd").await.unwrap().is_none());

    // The host's worktree pushes to a bare remote.
    let cwd = host
        .prepare_worktree(
            HostId::generate(),
            Worktree::Folder {
                task_id: TaskId::generate(),
            },
        )
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

    let files = vec![
        (
            "SKILL.md".to_owned(),
            b"---\nname: tdd\n---\nRed, green.\n".to_vec(),
        ),
        ("refs/it's.md".to_owned(), b"quote $HOME `x`\n".to_vec()),
    ];
    let got = engine
        .install_skill_pr(repo.id, "tdd", "1.0.0", &files, None)
        .await
        .unwrap();
    assert_eq!(got.branch, "sun/skill-tdd");
    let show = |path: &str| {
        git(
            dir.path(),
            &format!("--git-dir remote.git show sun/skill-tdd:{path}"),
        )
    };
    for folder in [".claude/skills", ".agents/skills"] {
        assert_eq!(
            show(&format!("{folder}/tdd/SKILL.md")),
            "---\nname: tdd\n---\nRed, green."
        );
        assert_eq!(
            show(&format!("'{folder}/tdd/refs/it'\\''s.md'")),
            "quote $HOME `x`"
        );
    }
    let pr = delivered(&engine, repo.id).await;
    assert_eq!(pr.pr.unwrap().url.as_deref(), Some(pr_url));
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
        (&json!("acme/web"), &json!("sun/skill-tdd"), &json!("main"))
    );
    // The anchor task is done; its run carries the repo.
    let task = engine.task(pr.task_id).await.unwrap();
    assert_eq!(task.status, TaskStatus::Done);
    let runs = engine.runs_of_task(pr.task_id).await.unwrap();
    assert_eq!(runs[0].repo_ids, vec![repo.id]);

    // Same version again: the same PR, nothing pushed or enqueued.
    let head = git(dir.path(), "--git-dir remote.git rev-parse sun/skill-tdd");
    let again = engine
        .install_skill_pr(repo.id, "tdd", "1.0.0", &files, None)
        .await
        .unwrap();
    assert_eq!(again.task_id, pr.task_id);
    assert_eq!(mods.calls.lock().unwrap().len(), 1);

    // A new version, for Codex only: one more commit on the same branch.
    let codex = ["codex".to_owned()];
    let newer = vec![("SKILL.md".to_owned(), b"v2\n".to_vec())];
    engine
        .install_skill_pr(repo.id, "tdd", "1.1.0", &newer, Some(&codex))
        .await
        .unwrap();
    assert_eq!(show(".agents/skills/tdd/SKILL.md"), "v2");
    let count = git(
        dir.path(),
        &format!("--git-dir remote.git rev-list --count {head}..sun/skill-tdd"),
    );
    assert_eq!(count, "1");
    let deadline = Instant::now() + Duration::from_secs(10);
    while mods.calls.lock().unwrap().len() < 2 {
        assert!(Instant::now() < deadline, "second open_pr never sent");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    // Bad input never reaches the host.
    let aider = ["aider".to_owned()];
    assert!(
        engine
            .install_skill_pr(repo.id, "tdd", "1.2.0", &newer, Some(&aider))
            .await
            .is_err()
    );
    let escape = vec![("../x.md".to_owned(), b"x".to_vec())];
    assert!(
        engine
            .install_skill_pr(repo.id, "tdd", "1.2.0", &escape, None)
            .await
            .is_err()
    );
}
