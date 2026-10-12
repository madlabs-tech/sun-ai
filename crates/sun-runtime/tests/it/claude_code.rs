//! Claude Code adapter: replays every recorded fixture, then drives a fake
//! `claude` script through the real process path. No real CLI is called.
// Test helpers outside #[test] fns aren't covered by clippy.toml's allow.
#![allow(clippy::unwrap_used)]

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use chrono::{TimeZone, Utc};
use serde_json::{Value, json};
use sun_core::{
    BoxStream, HostCtx, HostId, LimitKind, Role, RunId, RuntimeAdapter, SessionId, SessionSpec,
    Steer, StepKey, StopMode, SunEvent, Todo, TodoStatus,
};
use sun_runtime::ClaudeCode;
use sun_runtime::claude::{ClaudeStream, native_session_id};
use sun_runtime::process::{args_hash, replay};

use crate::common;

fn fixtures() -> String {
    common::pinned_dir("claude-code")
}

fn fixture(case: &str) -> (String, Option<i32>) {
    let jsonl = std::fs::read_to_string(format!("{}/{case}.jsonl", fixtures())).unwrap();
    let meta: Value = serde_json::from_str(
        &std::fs::read_to_string(format!("{}/{case}.meta.json", fixtures())).unwrap(),
    )
    .unwrap();
    (jsonl, meta["exit_code"].as_i64().map(|c| c as i32))
}

fn replay_case(case: &str) -> Vec<SunEvent> {
    let (jsonl, code) = fixture(case);
    replay("claude-code", ClaudeStream::default(), &jsonl, code, "")
}

fn write_call() -> SunEvent {
    SunEvent::ToolCall {
        name: "Write".into(),
        args_hash: args_hash(&json!({"file_path": "/work/hello.txt", "content": "hi"})),
        summary: "Write /work/hello.txt".into(),
    }
}

fn write_result() -> SunEvent {
    SunEvent::ToolResult {
        ok: true,
        summary: "File created successfully at: /work/hello.txt (file state is current in your context — no need to Read it back)".into(),
    }
}

fn context(tokens: u32) -> SunEvent {
    SunEvent::Usage {
        input: 0,
        output: 0,
        cost_usd: None,
        context_used: Some(tokens),
    }
}

#[test]
fn replays_normal_run() {
    assert_eq!(
        replay_case("normal"),
        vec![
            context(10 + 7478 + 28700),
            write_call(),
            write_result(),
            SunEvent::FileChanged {
                path: "hello.txt".into()
            },
            context(8 + 264 + 36178),
            SunEvent::Text {
                role: Role::Assistant,
                text: "Done.".into()
            },
            SunEvent::Usage {
                input: 18 + 7742 + 64878,
                output: 204,
                cost_usd: Some(0.0230098),
                context_used: None
            },
            SunEvent::DoneClaimed {
                summary: "Done.".into()
            },
            SunEvent::Exited { code: Some(0) },
        ]
    );
}

#[test]
fn replays_tool_error() {
    let events = replay_case("tool-error");
    assert!(events.contains(&SunEvent::ToolResult {
        ok: false,
        summary: "File does not exist. Note: your current working directory is /work.".into(),
    }));
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, SunEvent::FileChanged { .. } | SunEvent::Error { .. }))
    );
    assert_eq!(
        events[events.len() - 2..],
        [
            SunEvent::DoneClaimed {
                summary: "File does not exist at `/work/missing.txt`.".into()
            },
            SunEvent::Exited { code: Some(0) },
        ]
    );
}

#[test]
fn replays_limit_hit_once_with_reset_time() {
    let events = replay_case("limit-hit");
    let limits: Vec<_> = events
        .iter()
        .filter(|e| matches!(e, SunEvent::LimitHit { .. }))
        .collect();
    assert_eq!(limits.len(), 1, "{events:#?}");
    let SunEvent::LimitHit {
        limit,
        reset_at,
        raw,
    } = limits[0]
    else {
        unreachable!()
    };
    assert_eq!(*limit, LimitKind::Quota);
    assert_eq!(*reset_at, Utc.timestamp_opt(1_791_323_400, 0).single());
    assert!(raw.contains("five_hour"), "{raw}");
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, SunEvent::Error { .. } | SunEvent::DoneClaimed { .. }))
    );
    assert_eq!(events.last(), Some(&SunEvent::Exited { code: Some(1) }));
}

#[test]
fn replays_crash_as_error_then_exit() {
    let events = replay_case("crash");
    assert!(events.contains(&write_call()));
    assert!(events.contains(&SunEvent::FileChanged {
        path: "hello.txt".into()
    }));
    assert_eq!(
        events[events.len() - 2..],
        [
            SunEvent::Error {
                message: "claude-code exited with code 137 before finishing the turn".into()
            },
            SunEvent::Exited { code: Some(137) },
        ]
    );
}

#[test]
fn limit_text_on_stderr_beats_generic_error() {
    let events = replay(
        "claude-code",
        ClaudeStream::default(),
        "",
        Some(1),
        "Claude AI usage limit reached|1791323400\n",
    );
    assert_eq!(
        events[0],
        SunEvent::LimitHit {
            limit: LimitKind::Quota,
            reset_at: Utc.timestamp_opt(1_791_323_400, 0).single(),
            raw: "Claude AI usage limit reached|1791323400".into(),
        }
    );
}

#[test]
fn todo_write_becomes_plan() {
    let line = json!({"type": "assistant", "message": {"id": "m1", "content": [{
    "type": "tool_use", "name": "TodoWrite", "input": {"todos": [
        {"content": "Write the test", "status": "completed", "activeForm": "Writing"},
        {"content": "Fix the bug", "status": "in_progress", "activeForm": "Fixing"},
        {"content": "Open the PR", "status": "pending", "activeForm": "Opening"},
    ]}}]}});
    let events = replay(
        "claude-code",
        ClaudeStream::default(),
        &line.to_string(),
        Some(0),
        "",
    );
    assert_eq!(
        events[1],
        SunEvent::Plan {
            todos: vec![
                Todo {
                    text: "Write the test".into(),
                    status: TodoStatus::Done
                },
                Todo {
                    text: "Fix the bug".into(),
                    status: TodoStatus::InProgress
                },
                Todo {
                    text: "Open the PR".into(),
                    status: TodoStatus::Pending
                },
            ]
        }
    );
}

// --- the live process path, with a fake `claude` -------------------------

const FAKE_CLAUDE: &str = r#"#!/bin/sh
case "$1" in
  --version) echo "2.1.291 (Claude Code)"; exit 0 ;;
  auth) echo '{"loggedIn": true, "authMethod": "claude.ai"}'; exit 0 ;;
esac
echo "$@" > "$SUN_OUT.args"
read -r first
printf '%s\n' "$first" > "$SUN_OUT.stdin"
case "$SUN_MODE" in
  sleep) sleep 300 & echo $! > "$SUN_OUT.pid"; exec sleep 30 ;;
  steer)
    read -r second
    printf '%s\n' "$second" >> "$SUN_OUT.stdin"
    echo '{"type":"result","is_error":false,"result":"one"}'
    echo '{"type":"result","is_error":false,"result":"two"}' ;;
  secret) echo "{\"type\":\"result\",\"is_error\":false,\"result\":\"key is $API_TOKEN\"}" ;;
  *) cat "$SUN_FIXTURE" ;;
esac
exit "${SUN_EXIT:-0}"
"#;

/// A fresh dir with the fake `claude` script in it.
fn sandbox(name: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("claude-{name}"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let script = dir.join("claude");
    std::fs::write(&script, FAKE_CLAUDE).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    dir
}

fn spec(dir: &Path, env: &[(&str, &str)]) -> SessionSpec {
    let mut vars: BTreeMap<String, String> = env
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
    vars.insert("SUN_OUT".into(), dir.join("out").display().to_string());
    SessionSpec {
        session_id: SessionId::generate(),
        run_id: RunId::generate(),
        step_key: StepKey::new("implement").unwrap(),
        worktree: dir.to_path_buf(),
        envelope: "Create hello.txt".into(),
        envelope_parts: None,
        model: Some("haiku".into()),
        mcp_config: None,
        env: vars,
        exec_env: Default::default(),
        resume: None,
        permission_mode: None,
        byo_key: None,
    }
}

async fn collect(mut stream: BoxStream<'static, SunEvent>) -> Vec<SunEvent> {
    let all = async {
        let mut out = Vec::new();
        while let Some(event) = std::future::poll_fn(|cx| stream.as_mut().poll_next(cx)).await {
            out.push(event);
        }
        out
    };
    tokio::time::timeout(Duration::from_secs(20), all)
        .await
        .unwrap()
}

#[tokio::test]
async fn live_run_matches_replay_and_passes_the_envelope() {
    let dir = sandbox("live");
    let adapter = ClaudeCode::new(dir.join("claude"));
    let fixture = format!("{}/normal.jsonl", fixtures());
    let mut spec = spec(
        &dir,
        &[
            ("SUN_FIXTURE", &fixture),
            ("API_TOKEN", "s3cret-value-123456"),
        ],
    );
    spec.mcp_config = Some(dir.join("mcp.json"));
    let session_id = spec.session_id;
    let handle = adapter.start(spec).await.unwrap();
    assert_eq!(handle.native_id, Some(native_session_id(session_id)));

    let events = collect(adapter.events(&handle)).await;
    assert_eq!(events, replay_case("normal"));
    assert_eq!(adapter.export(&handle).await.unwrap().events, events);
    assert!(
        collect(adapter.events(&handle)).await.is_empty(),
        "events are yielded once"
    );

    let args = std::fs::read_to_string(dir.join("out.args")).unwrap();
    assert!(
        args.contains("--input-format stream-json --output-format stream-json --verbose"),
        "{args}"
    );
    assert!(
        args.contains(&format!("--session-id {}", native_session_id(session_id))),
        "{args}"
    );
    assert!(args.contains("--model haiku"), "{args}");
    assert!(args.contains("--permission-mode acceptEdits"), "{args}");
    // Sun's own MCP tools run without a permission prompt.
    assert!(args.contains("--allowedTools mcp__sun --model"), "{args}");
    let stdin: Value =
        serde_json::from_str(&std::fs::read_to_string(dir.join("out.stdin")).unwrap()).unwrap();
    assert_eq!(stdin["message"]["content"][0]["text"], "Create hello.txt");

    // The turn is done and stdin is closed: steering now fails, stop is a no-op.
    assert!(
        adapter
            .send(
                &handle,
                Steer {
                    text: "more".into()
                }
            )
            .await
            .is_err()
    );
    adapter.stop(&handle, StopMode::Graceful).await.unwrap();
}

#[tokio::test]
async fn live_crash_reports_error_then_exit() {
    let dir = sandbox("crash");
    let adapter = ClaudeCode::new(dir.join("claude"));
    let fixture = format!("{}/crash.jsonl", fixtures());
    let handle = adapter
        .start(spec(
            &dir,
            &[("SUN_FIXTURE", &fixture), ("SUN_EXIT", "137")],
        ))
        .await
        .unwrap();
    assert_eq!(collect(adapter.events(&handle)).await, replay_case("crash"));
}

#[tokio::test]
async fn start_resumes_an_earlier_conversation() {
    let dir = sandbox("resume");
    let adapter = ClaudeCode::new(dir.join("claude"));
    let fixture = format!("{}/normal.jsonl", fixtures());
    let mut spec = spec(&dir, &[("SUN_FIXTURE", &fixture)]);
    spec.resume = Some("0b9e2c1e-0000-4000-8000-000000000001".into());
    let handle = adapter.start(spec).await.unwrap();
    assert_eq!(
        handle.native_id.as_deref(),
        Some("0b9e2c1e-0000-4000-8000-000000000001")
    );
    collect(adapter.events(&handle)).await;
    let args = std::fs::read_to_string(dir.join("out.args")).unwrap();
    assert!(
        args.contains("--resume 0b9e2c1e-0000-4000-8000-000000000001"),
        "{args}"
    );
    assert!(!args.contains("--session-id"), "{args}");
}

#[tokio::test]
async fn steer_mid_turn_reaches_stdin() {
    let dir = sandbox("steer");
    let adapter = ClaudeCode::new(dir.join("claude"));
    let handle = adapter
        .start(spec(&dir, &[("SUN_MODE", "steer")]))
        .await
        .unwrap();
    adapter
        .send(
            &handle,
            Steer {
                text: "use snake_case".into(),
            },
        )
        .await
        .unwrap();
    let events = collect(adapter.events(&handle)).await;
    assert_eq!(
        events,
        vec![
            SunEvent::DoneClaimed {
                summary: "one".into()
            },
            SunEvent::DoneClaimed {
                summary: "two".into()
            },
            SunEvent::Exited { code: Some(0) },
        ]
    );
    let stdin = std::fs::read_to_string(dir.join("out.stdin")).unwrap();
    assert!(
        stdin.lines().nth(1).unwrap().contains("use snake_case"),
        "{stdin}"
    );
}

#[tokio::test]
async fn env_secrets_are_redacted_from_events() {
    let dir = sandbox("secret");
    let adapter = ClaudeCode::new(dir.join("claude"));
    let spec = spec(
        &dir,
        &[
            ("SUN_MODE", "secret"),
            ("API_TOKEN", "zq-not-a-real-value-42"),
        ],
    );
    let handle = adapter.start(spec).await.unwrap();
    let events = collect(adapter.events(&handle)).await;
    let SunEvent::DoneClaimed { summary } = &events[0] else {
        panic!("{events:?}")
    };
    assert!(
        summary.starts_with("key is ") && !summary.contains("zq-not-a-real"),
        "{summary}"
    );
}

#[tokio::test]
async fn kill_ends_the_stream_without_an_error() {
    let dir = sandbox("kill");
    let adapter = ClaudeCode::new(dir.join("claude"));
    let handle = adapter
        .start(spec(&dir, &[("SUN_MODE", "sleep")]))
        .await
        .unwrap();
    let events = adapter.events(&handle);
    // The CLI's own child (a background job holding stdout) dies with it.
    let grandchild = loop {
        match std::fs::read_to_string(dir.join("out.pid")) {
            Ok(pid) if pid.ends_with('\n') => break pid,
            _ => tokio::time::sleep(Duration::from_millis(20)).await,
        }
    };
    adapter.stop(&handle, StopMode::Kill).await.unwrap();
    adapter.stop(&handle, StopMode::Kill).await.unwrap();
    assert_eq!(collect(events).await, vec![SunEvent::Exited { code: None }]);
    let ps = std::process::Command::new("ps")
        .args(["-o", "stat=", "-p", grandchild.trim()])
        .output()
        .unwrap();
    let stat = String::from_utf8_lossy(&ps.stdout);
    assert!(
        stat.trim().is_empty() || stat.trim_start().starts_with('Z'),
        "grandchild {grandchild} still running: {stat}"
    );
}

#[tokio::test]
async fn readiness_reports_version_and_login() {
    let dir = sandbox("ready");
    let host = HostCtx {
        host_id: HostId::generate(),
    };
    let ready = ClaudeCode::new(dir.join("claude")).readiness(&host).await;
    assert!(ready.installed && ready.logged_in, "{ready:?}");
    assert_eq!(ready.version.as_deref(), Some("2.1.291"));
    assert_eq!(ready.detail, None);

    // An untested version still runs, with a warning naming the pinned fallback.
    let newer = sandbox("ready-newer");
    let script = FAKE_CLAUDE.replace("2.1.291", "9.9.0");
    std::fs::write(newer.join("claude"), script).unwrap();
    let ready = ClaudeCode::new(newer.join("claude")).readiness(&host).await;
    assert!(ready.installed && ready.logged_in, "{ready:?}");
    let pinned = sun_runtime::process::tested_versions("claude-code")[0];
    assert_eq!(
        ready.detail.unwrap(),
        format!(
            "Claude Code 9.9.0 is untested. If it misbehaves, install the known-good {pinned}: \
             npm install -g @anthropic-ai/claude-code@{pinned}"
        )
    );

    let missing = ClaudeCode::new(dir.join("not-there"))
        .readiness(&host)
        .await;
    assert!(!missing.installed && !missing.logged_in);
    assert!(
        missing
            .detail
            .unwrap()
            .contains("npm install -g @anthropic-ai/claude-code")
    );
}

#[test]
fn context_windows() {
    let adapter = ClaudeCode::default();
    assert_eq!(
        adapter.context_window("claude-haiku-4-5-20251001"),
        Some(200_000)
    );
    assert_eq!(adapter.context_window("sonnet[1m]"), Some(1_000_000));
    assert_eq!(adapter.context_window("gpt-5"), None);
}

#[tokio::test]
async fn envelope_user_template_reaches_the_first_prompt() {
    let dir = sandbox("template");
    let templates = dir.join("templates");
    std::fs::create_dir_all(&templates).unwrap();
    std::fs::write(
        templates.join("claude-code.md"),
        "Do: {{task}}\nRules:\n{{constraints}}\nChecks:\n{{gates}}\nPrior: {{handoff}}",
    )
    .unwrap();
    let mut adapter = ClaudeCode::new(dir.join("claude"));
    adapter.template_dir = Some(templates.clone());
    let fixture = format!("{}/normal.jsonl", fixtures());
    let mut spec = spec(&dir, &[("SUN_FIXTURE", &fixture)]);
    spec.envelope_parts = Some(sun_runtime::envelope::Envelope {
        task: "Create hello.txt".into(),
        constraints: vec!["No new dependencies.".into()],
        gates: vec!["cargo test".into()],
        context: vec![],
        handoff: None,
    });
    let handle = adapter.start(spec.clone()).await.unwrap();
    collect(adapter.events(&handle)).await;
    let stdin: Value =
        serde_json::from_str(&std::fs::read_to_string(dir.join("out.stdin")).unwrap()).unwrap();
    assert_eq!(
        stdin["message"]["content"][0]["text"],
        "Do: Create hello.txt\nRules:\n- No new dependencies.\nChecks:\n- cargo test\nPrior: None."
    );

    // A typo in the user's template fails the start, not the agent.
    std::fs::write(templates.join("claude-code.md"), "{{task}} {{gate}}").unwrap();
    let err = adapter.start(spec).await.unwrap_err();
    assert!(err.to_string().contains("{{gate}}"), "{err}");
}
