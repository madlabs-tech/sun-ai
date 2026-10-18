//! BLO-168 end to end: the kernel, the real `sun-host` and the real Codex
//! adapter on a fake `codex` that logs every argv. A session in a mode the
//! CLI has no exact equivalent for is refused by the host before the CLI
//! starts, whatever the kernel thought, and the run fails once, visibly,
//! with an error that names only the tool, the modes and the fix.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use sun_core::model::Run;
use sun_core::{PermissionMode, StateMachine, TaskId};
use sun_store::{EventLog, HostRepo, RunRepo};

use crate::api::until_online;
use crate::common::{Kernel, SECRET, TOKEN, kernel};
use crate::workspaces::call;

const MAIN: &str = "00000000000000000000000001";

/// Writes an executable `path` (through `sh`, so no handle of ours is still
/// open for writing when it first runs).
fn script(path: &Path, text: &str) {
    use std::io::Write;
    let mut sh = std::process::Command::new("sh")
        .args(["-c", "cat > \"$0\" && chmod 755 \"$0\""])
        .arg(path)
        .stdin(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    sh.stdin.take().unwrap().write_all(text.as_bytes()).unwrap();
    assert!(sh.wait().unwrap().success());
}

/// A real host on `k` whose only tool is Codex on a fake CLI (no
/// `--approve-for-me`, so it reports `default, plan, bypass_permissions`),
/// with `runtimes.permission_mode: accept_edits` as `sun.yaml` ships it.
/// Returns the fake's argv log and the host's data dir.
async fn codex_host(k: &Kernel, dir: &Path) -> (PathBuf, PathBuf) {
    let log = dir.join("argv.log");
    let codex = dir.join("codex");
    let version = sun_runtime::process::tested_versions("codex")[0];
    script(
        &codex,
        &format!(
            "#!/bin/sh\nprintf '%s\\n' \"$*\" >> '{log}'\n\
             case \"$1 $2\" in\n\
             '--version '*) echo 'codex-cli {version}'; exit 0 ;;\n\
             'login status') exit 0 ;;\n\
             'exec --help') echo '  --json  Print events'; exit 0 ;;\n\
             exec*) ;;\n\
             *) exit 1 ;;\n\
             esac\n\
             echo '{{\"type\":\"thread.started\",\"thread_id\":\"t-1\"}}'\nexec sleep 5\n",
            log = log.display()
        ),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("ws://{}/host", listener.local_addr().unwrap());
    tokio::spawn(axum::serve(listener, k.app.clone()).into_future());
    let host_dir = dir.join("host");
    let mut config = sun_host::HostConfig::new(&host_dir);
    config.permission_mode = Some(PermissionMode::AcceptEdits);
    let adapter = sun_runtime::codex::Codex::new(&codex);
    let host = sun_host::Host::open(config, vec![Arc::new(adapter)]).unwrap();
    tokio::spawn(host.run(sun_host::Credentials {
        kernel_url: url,
        host_id: k.host,
        host_key: host_key(k),
    }));
    until_online(&k.app, k.host).await;
    (log, host_dir)
}

/// `k`'s host key, as the kernel derives it from [`SECRET`].
fn host_key(k: &Kernel) -> String {
    Sha256::digest(format!("{SECRET}:{}", k.host).as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// A task assigned to a new Codex agent whose mode is `mode` (`None`: the
/// host's default).
async fn codex_agent_task(k: &Kernel, mode: Option<&str>) -> String {
    let (status, agent) = call(
        &k.app,
        "POST",
        "/api/v1/agents",
        Some(json!({ "name": "Cody", "runtime_chain": ["codex"], "permission_mode": mode })),
    )
    .await;
    assert!(status.is_success(), "{status} {agent}");
    let assignee = json!({ "kind": "agent", "id": agent["id"] });
    let (status, task) = call(
        &k.app,
        "POST",
        "/api/v1/tasks",
        Some(json!({ "title": "Fix it", "workspace_id": MAIN, "assignee": assignee })),
    )
    .await;
    assert!(status.is_success(), "{status} {task}");
    task["id"].as_str().unwrap().to_owned()
}

/// The task's runs once its latest one has ended.
async fn ended_runs(k: &Kernel, task: &str) -> Vec<Run> {
    let task: TaskId = task.parse().unwrap();
    for _ in 0..1000 {
        let runs = k
            .store
            .read()
            .await
            .unwrap()
            .runs_of_task(task)
            .await
            .unwrap();
        if runs.last().is_some_and(|r| r.status.is_terminal()) {
            return runs;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("task {task}'s run never ended");
}

/// Every event of `run`'s journal.
async fn events_of(k: &Kernel, run: &Run) -> Vec<sun_core::model::EventRecord> {
    let mut read = k.store.read().await.unwrap();
    read.events_after(Some(run.id), 0, 10_000).await.unwrap()
}

/// The fake CLI's argv lines that started a session: everything but the
/// readiness, effort and mode probes.
fn sessions_spawned(log: &Path) -> Vec<String> {
    let argv = std::fs::read_to_string(log).unwrap_or_default();
    argv.lines()
        .filter(|l| l.starts_with("exec") && *l != "exec --help")
        .map(str::to_owned)
        .collect()
}

/// Case 6: the refusal names the tool, the mode, the modes on offer and
/// the fix, and nothing else: no path, env value, key or token.
fn assert_clean(reason: &str, k: &Kernel, dirs: &[&Path]) {
    for dir in dirs {
        let dir = dir.display().to_string();
        assert!(!reason.contains(&dir), "a path leaked: {reason}");
    }
    for secret in [SECRET, TOKEN, host_key(k).as_str()] {
        assert!(!reason.contains(secret), "a secret leaked: {reason}");
    }
    for value in ["HOME", "PATH", "/tmp", "/home"] {
        assert!(!reason.contains(value), "`{value}` leaked: {reason}");
    }
}

/// Wait out the first dispatch retry (1 s), so a retried start would show.
async fn past_a_retry() {
    tokio::time::sleep(Duration::from_millis(1500)).await;
}

/// Case 1: a Codex agent on "host default", on a host whose default is the
/// stock `accept_edits`. Codex has no exact `accept_edits`, so the host
/// refuses before the CLI spawns: the run fails at once with the host's
/// error, is not retried, and no `workspace-write` session ever starts.
#[tokio::test(flavor = "multi_thread")]
async fn a_codex_agent_on_the_stock_host_default_fails_visibly_without_fallback() {
    let k = kernel().await;
    let dir = tempfile::tempdir().unwrap();
    let (log, host_dir) = codex_host(&k, dir.path()).await;
    let task = codex_agent_task(&k, None).await;
    let runs = ended_runs(&k, &task).await;
    past_a_retry().await;

    let run = &runs[0];
    assert_eq!(run.status, sun_core::RunStatus::Failed);
    let reason = run.end_reason.clone().unwrap_or_default();
    assert_eq!(
        reason,
        "host `box` refused the session: this host refuses the session: \
         codex doesn't offer permission mode `accept_edits`; it offers default, plan, \
         bypass_permissions; it is this host's default (sun.yaml `runtimes.permission_mode`); \
         pick a mode on the agent"
    );
    assert_clean(&reason, &k, &[dir.path(), &host_dir]);
    let events = events_of(&k, run).await;
    let kinds: Vec<&str> = events.iter().map(|e| e.kind.as_str()).collect();
    assert!(!kinds.contains(&"host_unreachable"), "retried: {kinds:?}");
    let task_id: TaskId = task.parse().unwrap();
    let all = k.store.read().await.unwrap().runs_of_task(task_id).await;
    assert_eq!(all.unwrap().len(), 1, "no second run");
    assert_eq!(
        sessions_spawned(&log),
        Vec::<String>::new(),
        "no CLI session"
    );
    let argv = std::fs::read_to_string(&log).unwrap();
    assert!(!argv.contains("workspace-write"), "{argv}");
    let (_, view) = call(&k.app, "GET", &format!("/api/v1/tasks/{task}"), None).await;
    assert_eq!(view["status"], "failed", "visible on the board: {view}");
}

/// Case 5: placement is bypassed. The kernel's copy of the host's report
/// is stale and claims Codex offers `auto`, so the kernel places an `auto`
/// agent there. The host checks its own adapter and still refuses with
/// `forbidden`; the engine fails the run once and does not retry.
#[tokio::test(flavor = "multi_thread")]
async fn a_host_refuses_a_mode_its_stale_report_claimed() {
    let k = kernel().await;
    let dir = tempfile::tempdir().unwrap();
    let (log, host_dir) = codex_host(&k, dir.path()).await;
    let stale: Value = {
        let mut read = k.store.read().await.unwrap();
        let mut caps = read.get_host(k.host).await.unwrap().capabilities;
        let codex = caps["runtimes"]
            .as_array_mut()
            .unwrap()
            .iter_mut()
            .find(|r| r["adapter"] == "codex")
            .expect("the host reported codex");
        assert_eq!(
            codex["permission_modes"],
            json!(["default", "plan", "bypass_permissions"]),
            "the real report"
        );
        codex["permission_modes"] = json!(["default", "plan", "auto", "bypass_permissions"]);
        caps
    };
    let mut tx = k.store.begin().await.unwrap();
    tx.set_host_capabilities(k.host, &stale).await.unwrap();
    tx.commit().await.unwrap();

    let task = codex_agent_task(&k, Some("auto")).await;
    let runs = ended_runs(&k, &task).await;
    past_a_retry().await;

    let run = &runs[0];
    assert_eq!(run.status, sun_core::RunStatus::Failed);
    let events = events_of(&k, run).await;
    let kinds: Vec<&str> = events.iter().map(|e| e.kind.as_str()).collect();
    assert!(
        kinds.contains(&"run_placed"),
        "the kernel placed it: {kinds:?}"
    );
    assert!(
        !kinds.iter().any(|k| k.starts_with("placement_refused")),
        "{kinds:?}"
    );
    let reason = run.end_reason.clone().unwrap_or_default();
    assert_eq!(
        reason,
        "host `box` refused the session: this host refuses the session: \
         codex doesn't offer permission mode `auto`; it offers default, plan, \
         bypass_permissions; pick one of those on the agent"
    );
    assert_clean(&reason, &k, &[dir.path(), &host_dir]);
    assert!(!kinds.contains(&"host_unreachable"), "retried: {kinds:?}");
    let task_id: TaskId = task.parse().unwrap();
    let all = k.store.read().await.unwrap().runs_of_task(task_id).await;
    assert_eq!(all.unwrap().len(), 1, "no second run");
    assert_eq!(
        sessions_spawned(&log),
        Vec::<String>::new(),
        "no CLI session"
    );
}
