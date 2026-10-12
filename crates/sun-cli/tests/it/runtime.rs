//! `sun runtime add --byo-key` / `sun runtime check` through a running kernel,
//! `sun secret set` in a workspace (Main by default; value on stdin), and the
//! `sun task pause|resume|cancel` interventions and `sun task|runtime on-limit`.
#![allow(clippy::unwrap_used)]

use std::io::Write;
use std::path::Path;
use std::process::{Command, Output, Stdio};

use sun_cli::serve::{self, ServeOptions};

fn sun(data: &Path, base: &str, token: &str, args: &[&str], stdin: &str) -> Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_sun"))
        .args(args)
        .env("SUN_DATA_DIR", data)
        .env("SUN_URL", base)
        .env("SUN_TOKEN", token)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(stdin.as_bytes())
        .unwrap();
    child.wait_with_output().unwrap()
}

fn text(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned() + &String::from_utf8_lossy(&out.stderr)
}

/// `sun task pause` and `sun task resume` go through the kernel and both
/// land in the run's event log.
#[test]
fn task_pause_resume_journals_both() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("data");
    std::fs::create_dir_all(&data).unwrap();
    let rt = tokio::runtime::Runtime::new().unwrap();
    let kernel = rt
        .block_on(serve::start(
            ServeOptions {
                data_dir: data.clone(),
                with_host: false,
                listen: Some("127.0.0.1:0".into()),
                sandbox_mods: false,
                sun_exe: env!("CARGO_BIN_EXE_sun").into(),
                exec_env_tools: Default::default(),
            },
            vec![],
        ))
        .unwrap();
    let base = format!("http://{}", kernel.addr);
    let run = |args: &[&str]| {
        let out = sun(&data, &base, &kernel.token, args, "");
        assert!(out.status.success(), "{args:?}: {}", text(&out));
        text(&out)
    };
    let created = run(&["task", "create", "Pause me"]);
    let task = created.split_whitespace().next().unwrap().to_owned();
    let client = sun_cli::client::Client::new(&base, &kernel.token);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    let run_id = loop {
        let view = client.get(&format!("/tasks/{task}")).unwrap();
        if view["run"]["status"] == "running" {
            break view["run"]["id"].as_str().unwrap().to_owned();
        }
        assert!(std::time::Instant::now() < deadline, "never ran: {view}");
        std::thread::sleep(std::time::Duration::from_millis(20));
    };

    let paused = run(&["task", "pause", &task]);
    // The workspace shows a paused task as waiting on a human.
    assert!(paused.contains("waiting_approval"), "{paused}");
    assert!(run(&["task", "resume", &task]).contains("running"));
    let events = client.get(&format!("/runs/{run_id}/events")).unwrap();
    let moves: Vec<(&str, &str)> = events
        .as_array()
        .unwrap()
        .iter()
        .filter(|e| e["kind"] == "run_status")
        .map(|e| {
            let p = &e["payload"];
            (p["from"].as_str().unwrap_or(""), p["to"].as_str().unwrap())
        })
        .collect();
    assert!(moves.contains(&("running", "paused")), "{moves:?}");
    assert!(moves.contains(&("paused", "running")), "{moves:?}");
    // A task that isn't paused can't be resumed; the kernel says why.
    let out = sun(&data, &base, &kernel.token, &["task", "resume", &task], "");
    assert!(!out.status.success());
    assert!(text(&out).contains("not paused"), "{}", text(&out));

    // The limit policy per task and per ecosystem.
    assert!(run(&["task", "on-limit", &task]).contains("on_limit: default"));
    let set = run(&["task", "on-limit", &task, "pause_and_notify"]);
    assert!(set.contains("on_limit: pause_and_notify"), "{set}");
    assert!(run(&["task", "on-limit", &task]).contains("pause_and_notify"));
    assert!(run(&["task", "on-limit", &task, "default"]).contains("on_limit: default"));
    run(&["runtime", "on-limit", "wait_until_reset"]);
    assert!(run(&["runtime", "on-limit"]).contains("on_limit: wait_until_reset"));
    let out = sun(
        &data,
        &base,
        &kernel.token,
        &["runtime", "on-limit", "explode"],
        "",
    );
    assert!(!out.status.success(), "{}", text(&out));
    run(&["task", "cancel", &task]);
}

#[test]
fn byo_key_runtime_add_check_and_workspace_secret() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("data");
    std::fs::create_dir_all(&data).unwrap();
    let rt = tokio::runtime::Runtime::new().unwrap();
    let kernel = rt
        .block_on(serve::start(
            ServeOptions {
                data_dir: data.clone(),
                with_host: false,
                listen: Some("127.0.0.1:0".into()),
                sandbox_mods: false,
                sun_exe: env!("CARGO_BIN_EXE_sun").into(),
                exec_env_tools: Default::default(),
            },
            vec![],
        ))
        .unwrap();
    let base = format!("http://{}", kernel.addr);
    let run = |args: &[&str], stdin: &str| sun(&data, &base, &kernel.token, args, stdin);

    let add = [
        "runtime",
        "add",
        "openrouter",
        "--byo-key",
        "--endpoint",
        "https://openrouter.ai/api/v1",
        "--model",
        "openai/gpt-5",
        "--key-secret",
        "OR_KEY",
    ];
    let out = run(&add, "");
    assert!(out.status.success(), "{}", text(&out));
    let bad = [
        "runtime",
        "add",
        "x",
        "--byo-key",
        "--endpoint",
        "",
        "--model",
        "m",
    ];
    let out = run(&bad, "");
    assert!(!out.status.success());
    assert!(
        text(&out).contains("endpoint: no endpoint"),
        "{}",
        text(&out)
    );

    let out = run(&["runtime", "check", "openrouter"], "");
    assert!(!out.status.success());
    assert!(text(&out).contains("`OR_KEY` is not set"), "{}", text(&out));

    // The key goes in on stdin, into Main, where the runtime reads it.
    let out = run(&["secret", "set", "OR_KEY"], "sk-or-0123456789\n");
    assert!(
        text(&out).contains("workspace:00000000000000000000000001"),
        "{}",
        text(&out)
    );
    assert!(out.status.success(), "{}", text(&out));
    let out = run(&["secret", "list"], "");
    assert!(text(&out).contains("OR_KEY"), "{}", text(&out));
    assert!(!text(&out).contains("sk-or-0123456789"));
    let out = run(&["runtime", "check", "openrouter"], "");
    // Config and key are fine; no host runs OpenCode in this kernel.
    assert!(!text(&out).contains("is not set"), "{}", text(&out));
    assert!(
        text(&out).contains("no online host has opencode"),
        "{}",
        text(&out)
    );

    // Another workspace: its own runtime of that name, and its own key.
    let out = run(&["workspace", "create", "Ops"], "");
    assert!(out.status.success(), "{}", text(&out));
    let mut in_ops = add.to_vec();
    in_ops.extend(["--workspace", "ops"]);
    let out = run(&in_ops, "");
    assert!(out.status.success(), "{}", text(&out));
    let check = ["runtime", "check", "openrouter", "--workspace", "Ops"];
    let out = run(&check, "");
    assert!(
        text(&out).contains("`OR_KEY` is not set"),
        "Main's key isn't Ops': {}",
        text(&out)
    );
    let out = run(
        &["secret", "set", "OR_KEY", "--workspace", "ops"],
        "sk-or-ops-0123456789\n",
    );
    assert!(out.status.success(), "{}", text(&out));
    let out = run(&check, "");
    assert!(!text(&out).contains("is not set"), "{}", text(&out));
    // A deployment secret stays in the ecosystem.
    let out = run(&["secret", "set", "GITHUB_APP_SLUG"], "sun-app\n");
    assert!(text(&out).contains("(ecosystem:"), "{}", text(&out));
    let out = run(&["secret", "list"], "");
    let listed = text(&out);
    assert!(
        listed.contains("GITHUB_APP_SLUG") && listed.contains("OR_KEY"),
        "{listed}"
    );
}
