//! Codex adapter: exact fixture replays, then a fake `codex` script through
//! the real process path. No real CLI is called.
#![allow(clippy::unwrap_used)]

use crate::common;

use chrono::{TimeZone, Utc};
use common::{collect, parts, pinned_dir, replay_case, sandbox, spec};
use serde_json::json;
use sun_core::{
    HostCtx, HostId, LimitKind, Role, RuntimeAdapter, SessionId, Steer, StopMode, SunEvent, Todo,
    TodoStatus,
};
use sun_runtime::Codex;
use sun_runtime::codex::CodexStream;
use sun_runtime::contract::{Case, violations};
use sun_runtime::process::{args_hash, replay};

const WRITE: &str = r#"/bin/zsh -lc "printf 'hi' > hello.txt""#;

fn shell_call(command: &str) -> SunEvent {
    SunEvent::ToolCall {
        name: "shell".into(),
        args_hash: args_hash(&json!({ "command": command })),
        summary: format!("shell {command}"),
    }
}

#[test]
fn replays_normal_run() {
    assert_eq!(
        replay_case("codex", "normal"),
        [
            shell_call(WRITE),
            SunEvent::ToolResult {
                ok: true,
                summary: String::new()
            },
            SunEvent::Text {
                role: Role::Assistant,
                text: "done".into()
            },
            SunEvent::Usage {
                input: 34316,
                output: 44,
                cost_usd: None,
                context_used: None
            },
            SunEvent::DoneClaimed {
                summary: "done".into()
            },
            SunEvent::Exited { code: Some(0) },
        ]
    );
}

#[test]
fn replays_failed_command_as_failed_tool_result() {
    let events = replay_case("codex", "tool-error");
    assert_eq!(events[0], shell_call("/bin/zsh -lc 'cat missing.txt'"));
    assert_eq!(
        events[1],
        SunEvent::ToolResult {
            ok: false,
            summary: "cat: missing.txt: No such file or directory".into()
        }
    );
}

#[test]
fn replays_usage_limit_once_with_reset_time() {
    let events = replay_case("codex", "limit-hit");
    let SunEvent::LimitHit {
        limit, reset_at, ..
    } = &events[0]
    else {
        panic!("{events:#?}")
    };
    assert_eq!(*limit, LimitKind::Quota);
    assert_eq!(
        *reset_at,
        Utc.with_ymd_and_hms(2026, 10, 7, 23, 50, 0).single()
    );
    assert_eq!(events[1..], [SunEvent::Exited { code: Some(1) }]);
}

#[test]
fn replays_crash_as_error_then_exit() {
    let events = replay_case("codex", "crash");
    assert_eq!(
        events[events.len() - 2..],
        [
            SunEvent::Error {
                message: "codex exited with code 137 before finishing the turn: Reading additional input from stdin...".into()
            },
            SunEvent::Exited { code: Some(137) },
        ]
    );
}

#[test]
fn file_changes_and_todo_lists() {
    let jsonl = [
        json!({"type": "item.updated", "item": {"id": "t", "type": "todo_list", "items": [
            {"text": "Edit", "completed": true}, {"text": "Test", "completed": false}]}}),
        json!({"type": "item.completed", "item": {"id": "p", "type": "file_change", "status": "completed",
            "changes": [{"path": "/work/src/a.rs", "kind": "update"}]}}),
        json!({"type": "turn.completed", "usage": {}}),
    ]
    .map(|line| line.to_string())
    .join("\n");
    let events = replay(
        "codex",
        CodexStream::new(Some("/work".into()), None),
        &jsonl,
        Some(0),
        "",
    );
    assert_eq!(
        events[..4],
        [
            SunEvent::Plan {
                todos: vec![
                    Todo {
                        text: "Edit".into(),
                        status: TodoStatus::Done
                    },
                    Todo {
                        text: "Test".into(),
                        status: TodoStatus::Pending
                    },
                ]
            },
            SunEvent::ToolCall {
                name: "apply_patch".into(),
                args_hash: args_hash(
                    &json!({"changes": [{"path": "/work/src/a.rs", "kind": "update"}]})
                ),
                summary: "apply_patch /work/src/a.rs".into(),
            },
            SunEvent::ToolResult {
                ok: true,
                summary: String::new()
            },
            SunEvent::FileChanged {
                path: "src/a.rs".into()
            },
        ]
    );
}

// --- the live process path, with a fake `codex` --------------------------

const FAKE_CODEX: &str = r#"#!/bin/sh
case "$1" in
  --version) echo "codex-cli 0.160.0"; exit 0 ;;
  login) exit 0 ;;
  debug) [ "$2 $3" = "models --bundled" ] || exit 2; cat <<'MODELS'
{"models": [
  {"slug": "gpt-5-codex", "display_name": "GPT-5 Codex", "visibility": "list",
   "default_reasoning_level": "medium",
   "supported_reasoning_levels": [
     {"effort": "low", "description": "Fast responses with lighter reasoning"},
     {"effort": "medium", "description": "Balances speed and reasoning depth"},
     {"effort": "high", "description": "Greater reasoning depth"}]},
  {"slug": "gpt-5", "display_name": "GPT-5", "visibility": "list",
   "default_reasoning_level": "medium",
   "supported_reasoning_levels": [
     {"effort": "minimal"}, {"effort": "low"}, {"effort": "medium"},
     {"effort": "high"}, {"effort": "xhigh"}]},
  {"slug": "gpt-5.1-mini", "display_name": "GPT-5.1 Mini", "visibility": "list",
   "supported_reasoning_levels": []},
  {"slug": "internal", "display_name": "Hidden", "visibility": "hide",
   "supported_reasoning_levels": [{"effort": "low"}]},
  {"slug": "--danger", "visibility": "list", "supported_reasoning_levels": []}
]}
MODELS
  exit 0 ;;
esac
case " $* " in
  *" --skip-git-repo-check "*) ;;
  *) d=$PWD
     until [ -e "$d/.git" ]; do
       [ "$d" = / ] && { echo "Not inside a trusted directory and --skip-git-repo-check was not specified." >&2; exit 1; }
       d=$(dirname "$d")
     done ;;
esac
printf '%s\n' "$*" >> "$SUN_OUT.args"
[ -r /proc/$$/cmdline ] && tr '\0' ' ' < /proc/$$/cmdline >> "$SUN_OUT.cmdline"
printf '%s\n' "${SUN_MCP_TOKEN-}" >> "$SUN_OUT.token"
thread='{"type":"thread.started","thread_id":"t-1"}'
case "$*" in
  *" resume "*)
    echo "$thread"
    echo '{"type":"item.completed","item":{"id":"i","type":"agent_message","text":"two"}}'
    echo '{"type":"turn.completed","usage":{"input_tokens":2,"output_tokens":1}}'
    exit 0 ;;
esac
case "$SUN_MODE" in
  steer)
    echo "$thread"
    sleep 1
    echo '{"type":"item.completed","item":{"id":"i","type":"agent_message","text":"one"}}'
    echo '{"type":"turn.completed","usage":{"input_tokens":2,"output_tokens":1}}' ;;
  sleep) echo "$thread"; exec sleep 30 ;;
  fail)
    echo "$thread"
    while [ ! -e "$SUN_OUT.go" ]; do sleep 0.05; done
    echo '{"type":"turn.failed","error":{"message":"boom"}}'
    exit 1 ;;
  *) cat "$SUN_FIXTURE" ;;
esac
exit "${SUN_EXIT:-0}"
"#;

/// A fake-`codex` sandbox that is a git repo, as a repo-backed task's
/// worktree is.
fn repo_sandbox(name: &str) -> std::path::PathBuf {
    let dir = sandbox("codex", FAKE_CODEX, name);
    std::fs::create_dir(dir.join(".git")).unwrap();
    dir
}

#[tokio::test]
async fn live_run_matches_replay_and_passes_the_envelope() {
    let dir = repo_sandbox("live");
    let adapter = Codex::new(dir.join("codex"));
    let fixture = format!("{}/normal.jsonl", pinned_dir("codex"));
    let mut spec = spec(&dir, &[("SUN_FIXTURE", &fixture)]);
    spec.model = Some("gpt-5".into());
    spec.effort = Some("xhigh".into());
    let handle = adapter.start(spec).await.unwrap();
    assert_eq!(
        handle.native_id.as_deref(),
        Some("01a11281-ab38-7922-8560-110df8258cd1")
    );
    let events = collect(adapter.events(&handle)).await;
    assert_eq!(events, replay_case("codex", "normal"));
    assert_eq!(adapter.export(&handle).await.unwrap().events, events);
    let args = std::fs::read_to_string(dir.join("out.args")).unwrap();
    assert_eq!(
        args,
        "exec --json -c sandbox_mode=\"workspace-write\" -c model=\"gpt-5\" -c model_reasoning_effort=\"xhigh\" -- Create hello.txt\n"
    );
    let steer = Steer {
        text: "more".into(),
    };
    assert!(adapter.send(&handle, steer).await.is_err(), "session ended");
}

/// A fresh task folder outside any git repo, as a repo-less task gets.
fn folder_task(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("sun-codex-{name}-{}", SessionId::generate()));
    std::fs::create_dir_all(&dir).unwrap();
    assert!(
        !dir.ancestors().any(|d| d.join(".git").exists()),
        "{} is inside a git repo",
        dir.display()
    );
    dir
}

#[tokio::test]
async fn fake_codex_rejects_a_dir_outside_git() {
    let dir = repo_sandbox("untrusted");
    let folder = folder_task("untrusted");
    let out = std::process::Command::new(dir.join("codex"))
        .args(["exec", "--json", "--", "hi"])
        .current_dir(&folder)
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&out.stderr).contains("Not inside a trusted directory"));
    std::fs::remove_dir_all(folder).unwrap();
}

#[tokio::test]
async fn folder_task_skips_the_git_repo_check_on_every_turn() {
    let dir = repo_sandbox("folder");
    let folder = folder_task("folder");
    let adapter = Codex::new(dir.join("codex"));
    let handle = adapter
        .start(spec(&folder, &[("SUN_MODE", "steer")]))
        .await
        .unwrap();
    let steer = Steer {
        text: "again".into(),
    };
    adapter.send(&handle, steer).await.unwrap();
    let events = collect(adapter.events(&handle)).await;
    assert_eq!(events.last(), Some(&SunEvent::Exited { code: Some(0) }));
    let args = std::fs::read_to_string(folder.join("out.args")).unwrap();
    let turns: Vec<&str> = args.lines().collect();
    assert_eq!(turns.len(), 2, "{args}");
    for turn in &turns {
        assert!(
            turn.starts_with("exec --skip-git-repo-check --json -c "),
            "{args}"
        );
    }
    assert!(turns[1].ends_with("resume t-1 -- again"), "{args}");
    std::fs::remove_dir_all(folder).unwrap();
}

#[tokio::test]
async fn repo_task_keeps_the_git_repo_check() {
    let dir = repo_sandbox("repo");
    let fixture = format!("{}/normal.jsonl", pinned_dir("codex"));
    let adapter = Codex::new(dir.join("codex"));
    let handle = adapter
        .start(spec(&dir, &[("SUN_FIXTURE", &fixture)]))
        .await
        .unwrap();
    collect(adapter.events(&handle)).await;
    let args = std::fs::read_to_string(dir.join("out.args")).unwrap();
    assert!(!args.contains("--skip-git-repo-check"), "{args}");
}

/// The session's MCP token reaches Codex through its env and `env_vars`,
/// never argv (`ps` and `/proc/<pid>/cmdline` are readable by every user).
#[tokio::test]
async fn mcp_token_stays_off_argv() {
    const CANARY: &str = "canary-7f3c1e0b9d24";
    let dir = repo_sandbox("mcp");
    let server = |args: &[&str]| json!({"command": "/bin/sun", "args": args, "env": {"SUN_MCP_TOKEN": CANARY}});
    let config = json!({"mcpServers": {
        "sun": server(&["mcp"]),
        "docs": server(&["mcp", "relay", "{}"]),
    }});
    std::fs::write(dir.join("mcp.json"), config.to_string()).unwrap();
    let fixture = format!("{}/normal.jsonl", pinned_dir("codex"));
    let mut spec = spec(&dir, &[("SUN_FIXTURE", &fixture)]);
    spec.mcp_config = Some(dir.join("mcp.json"));
    let adapter = Codex::new(dir.join("codex"));
    let handle = adapter.start(spec).await.unwrap();
    collect(adapter.events(&handle)).await;

    let args = std::fs::read_to_string(dir.join("out.args")).unwrap();
    assert!(!args.contains(CANARY), "{args}");
    for name in ["sun", "docs"] {
        let names = format!(r#"-c mcp_servers.{name}.env_vars=["SUN_MCP_TOKEN"]"#);
        assert!(args.contains(&names), "{args}");
    }
    if cfg!(target_os = "linux") {
        let cmdline = std::fs::read_to_string(dir.join("out.cmdline")).unwrap();
        assert!(cmdline.contains("exec --json"), "{cmdline}");
        assert!(!cmdline.contains(CANARY), "{cmdline}");
    }
    let token = std::fs::read_to_string(dir.join("out.token")).unwrap();
    assert_eq!(token, format!("{CANARY}\n"));
}

#[tokio::test]
async fn live_crash_honors_the_contract() {
    let dir = repo_sandbox("crash");
    let adapter = Codex::new(dir.join("codex"));
    let fixture = format!("{}/crash.jsonl", pinned_dir("codex"));
    let spec = spec(&dir, &[("SUN_FIXTURE", &fixture), ("SUN_EXIT", "137")]);
    let handle = adapter.start(spec).await.unwrap();
    let events = collect(adapter.events(&handle)).await;
    assert_eq!(violations(Case::Crash, &events), Vec::<String>::new());
}

#[tokio::test]
async fn steer_resumes_the_thread_after_the_turn() {
    let dir = repo_sandbox("steer");
    let adapter = Codex::new(dir.join("codex"));
    let handle = adapter
        .start(spec(&dir, &[("SUN_MODE", "steer")]))
        .await
        .unwrap();
    let steer = Steer {
        text: "use snake_case".into(),
    };
    adapter.send(&handle, steer).await.unwrap();
    let events = collect(adapter.events(&handle)).await;
    let usage = SunEvent::Usage {
        input: 2,
        output: 1,
        cost_usd: None,
        context_used: None,
    };
    let turn = |text: &str| {
        [
            SunEvent::Text {
                role: Role::Assistant,
                text: text.into(),
            },
            usage.clone(),
            SunEvent::DoneClaimed {
                summary: text.into(),
            },
        ]
    };
    let mut want = turn("one").to_vec();
    want.extend(turn("two"));
    want.push(SunEvent::Exited { code: Some(0) });
    assert_eq!(events, want);
    let args = std::fs::read_to_string(dir.join("out.args")).unwrap();
    assert!(
        args.lines()
            .nth(1)
            .unwrap()
            .ends_with("resume t-1 -- use snake_case"),
        "{args}"
    );
}

#[tokio::test]
async fn steers_queued_on_a_failed_turn_are_reported() {
    let dir = repo_sandbox("fail");
    let adapter = Codex::new(dir.join("codex"));
    let handle = adapter
        .start(spec(&dir, &[("SUN_MODE", "fail")]))
        .await
        .unwrap();
    for text in ["one", "two"] {
        let steer = Steer { text: text.into() };
        adapter.send(&handle, steer).await.unwrap();
    }
    std::fs::write(dir.join("out.go"), "").unwrap();
    assert_eq!(
        collect(adapter.events(&handle)).await,
        [
            SunEvent::Error {
                message: "boom".into()
            },
            SunEvent::Error {
                message: "codex: the turn did not finish; dropped queued steers: one | two".into()
            },
            SunEvent::Exited { code: Some(1) },
        ]
    );
}

#[tokio::test]
async fn kill_ends_the_stream_without_an_error() {
    let dir = repo_sandbox("kill");
    let adapter = Codex::new(dir.join("codex"));
    let handle = adapter
        .start(spec(&dir, &[("SUN_MODE", "sleep")]))
        .await
        .unwrap();
    let events = adapter.events(&handle);
    adapter.stop(&handle, StopMode::Kill).await.unwrap();
    adapter.stop(&handle, StopMode::Kill).await.unwrap();
    assert_eq!(collect(events).await, [SunEvent::Exited { code: None }]);
}

#[tokio::test]
async fn readiness_reports_version_and_login() {
    let dir = repo_sandbox("ready");
    let host = HostCtx {
        host_id: HostId::generate(),
    };
    let ready = Codex::new(dir.join("codex")).readiness(&host).await;
    assert!(ready.installed && ready.logged_in, "{ready:?}");
    assert_eq!(ready.version.as_deref(), Some("0.160.0"));
    assert_eq!(ready.detail, None);
    let missing = Codex::new(dir.join("not-there")).readiness(&host).await;
    assert!(!missing.installed);
    assert!(
        missing
            .detail
            .unwrap()
            .contains("npm install -g @openai/codex")
    );
}

/// The effort probe reads `codex debug models --bundled` (Codex 0.122+; the
/// fake prints its JSON shape): per-model levels, and their union tool-wide.
#[tokio::test]
async fn effort_is_per_model_from_the_installed_cli() {
    let dir = repo_sandbox("effort");
    let host = HostCtx {
        host_id: HostId::generate(),
    };
    let catalog = Codex::new(dir.join("codex")).effort(&host).await;
    let values = |levels: &[sun_core::EffortLevel]| -> Vec<String> {
        levels.iter().map(|l| l.value.clone()).collect()
    };
    assert_eq!(
        values(&catalog.effort_options),
        ["minimal", "low", "medium", "high", "xhigh"]
    );
    let ids: Vec<&str> = catalog.models.iter().map(|m| m.id.as_str()).collect();
    assert_eq!(
        ids,
        ["gpt-5-codex", "gpt-5", "gpt-5.1-mini"],
        "hidden and bad ids left out"
    );
    let codex = &catalog.models[0];
    assert_eq!(codex.label, "GPT-5 Codex");
    assert_eq!(values(&codex.effort_options), ["low", "medium", "high"]);
    assert_eq!(codex.default_effort.as_deref(), Some("medium"));
    assert_eq!(
        codex.effort_options[2].description.as_deref(),
        Some("Greater reasoning depth")
    );
    assert_eq!(catalog.models[1].effort_options[4].label, "Extra high");
    assert!(catalog.models[2].effort_options.is_empty());

    // Too old for `debug models`, or not installed: none.
    let old = sandbox(
        "codex",
        &FAKE_CODEX.replace("  debug)", "  debug) exit 2 ;;\n  unused)"),
        "effort-old",
    );
    let none = sun_core::EffortCatalog::default();
    assert_eq!(Codex::new(old.join("codex")).effort(&host).await, none);
    assert_eq!(Codex::new(dir.join("not-there")).effort(&host).await, none);
    assert!(sun_runtime::codex::parse_debug_models(b"not json").is_empty());
}

#[tokio::test]
async fn start_resumes_an_earlier_thread() {
    let dir = repo_sandbox("resume");
    let adapter = Codex::new(dir.join("codex"));
    let mut spec = spec(&dir, &[]);
    spec.resume = Some("t-9".into());
    let handle = adapter.start(spec).await.unwrap();
    assert_eq!(handle.native_id.as_deref(), Some("t-9"));
    let events = collect(adapter.events(&handle)).await;
    assert!(
        matches!(
            events.as_slice(),
            [
                ..,
                SunEvent::DoneClaimed { .. },
                SunEvent::Exited { code: Some(0) }
            ]
        ),
        "{events:?}"
    );
    let args = std::fs::read_to_string(dir.join("out.args")).unwrap();
    assert!(args.ends_with("resume t-9 -- Create hello.txt\n"), "{args}");
}

#[tokio::test]
async fn builtin_template_reaches_the_first_prompt() {
    let dir = repo_sandbox("template");
    let adapter = Codex::new(dir.join("codex"));
    let fixture = format!("{}/normal.jsonl", pinned_dir("codex"));
    let mut spec = spec(&dir, &[("SUN_FIXTURE", &fixture)]);
    spec.envelope_parts = Some(parts());
    let handle = adapter.start(spec).await.unwrap();
    collect(adapter.events(&handle)).await;
    let args = std::fs::read_to_string(dir.join("out.args")).unwrap();
    assert!(args.contains("-- # Task\n\nCreate hello.txt\n"), "{args}");
    assert!(args.contains("- No new dependencies.\n"), "{args}");
    assert!(args.contains("with your plan tool"), "{args}");
}

#[tokio::test]
async fn an_effort_that_is_not_a_token_never_reaches_the_cli() {
    let dir = repo_sandbox("bad-effort");
    let adapter = Codex::new(dir.join("codex"));
    let mut spec = spec(&dir, &[]);
    spec.effort = Some("high\" -c sandbox_mode=\"danger-full-access".into());
    assert!(adapter.start(spec).await.is_err());
    assert!(!dir.join("out.args").exists(), "the CLI was not started");
}
