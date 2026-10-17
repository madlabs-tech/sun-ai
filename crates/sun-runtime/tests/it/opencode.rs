//! OpenCode adapter: exact fixture replays, then a fake `opencode acp`
//! script through the real process path. No real CLI is called.
// Test helpers outside #[test] fns aren't covered by clippy.toml's allow.
#![allow(clippy::unwrap_used)]

use crate::common;

use chrono::{Duration, Utc};
use common::{collect, fixture, parts, replay_case, sandbox, spec};
use serde_json::{Value, json};
use sun_core::{
    HostCtx, HostId, LimitKind, Role, RuntimeAdapter, Steer, StopMode, SunEvent, Todo, TodoStatus,
};
use sun_runtime::OpenCode;
use sun_runtime::opencode::AcpStream;
use sun_runtime::process::{args_hash, replay};

fn write_events() -> [SunEvent; 3] {
    [
        SunEvent::ToolCall {
            name: "write".into(),
            args_hash: args_hash(&json!({"filePath": "/work/hello.txt", "content": "hi"})),
            summary: "write hello.txt".into(),
        },
        SunEvent::ToolResult {
            ok: true,
            summary: "Wrote file successfully.".into(),
        },
        SunEvent::FileChanged {
            path: "hello.txt".into(),
        },
    ]
}

#[test]
fn replays_normal_run() {
    let mut want = write_events().to_vec();
    want.extend([
        SunEvent::Text {
            role: Role::Assistant,
            text: "done".into(),
        },
        SunEvent::Usage {
            input: 0,
            output: 0,
            cost_usd: Some(0.0),
            context_used: Some(7875),
        },
        SunEvent::Usage {
            input: 131 + 7744,
            output: 3 + 3,
            cost_usd: None,
            context_used: None,
        },
        SunEvent::DoneClaimed {
            summary: "done".into(),
        },
        SunEvent::Exited { code: Some(143) },
    ]);
    assert_eq!(replay_case("opencode", "normal"), want);
}

#[test]
fn replays_failed_read_without_file_change() {
    let events = replay_case("opencode", "tool-error");
    assert_eq!(
        events[..2],
        [
            SunEvent::ToolCall {
                name: "read".into(),
                args_hash: args_hash(&json!({"filePath": "/work/missing.txt"})),
                summary: "read missing.txt".into(),
            },
            SunEvent::ToolResult {
                ok: false,
                summary: "File not found: /work/missing.txt".into()
            },
        ]
    );
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, SunEvent::FileChanged { .. }))
    );
    // Streamed chunks are joined into one message.
    assert!(events.contains(&SunEvent::Text {
        role: Role::Assistant,
        text: "File not found: /private/var/folders/rm/6nlf170j0wbgy5603bb6f5bw0000gn/T/tmp.0uO72qpJKy/missing.txt does not exist.".into()
    }));
}

#[test]
fn replays_provider_429_as_rate_limit_with_retry_after() {
    let events = replay_case("opencode", "limit-hit");
    let SunEvent::LimitHit {
        limit,
        reset_at: Some(reset_at),
        raw,
    } = &events[0]
    else {
        panic!("{events:#?}")
    };
    assert_eq!(*limit, LimitKind::RateLimit);
    let in_an_hour = Utc::now() + Duration::hours(1);
    assert!(
        (*reset_at - in_an_hour).num_seconds().abs() < 60,
        "{reset_at}"
    );
    assert!(raw.contains("retry-after"), "{raw}");
    assert_eq!(events[1..], [SunEvent::Exited { code: Some(143) }]);
}

#[test]
fn replays_crash_as_error_then_exit() {
    let mut want = write_events().to_vec();
    want.extend([
        SunEvent::Error {
            message: "opencode exited with code 137 before finishing the turn".into(),
        },
        SunEvent::Exited { code: Some(137) },
    ]);
    assert_eq!(replay_case("opencode", "crash"), want);
}

#[test]
fn plan_updates_become_plans() {
    let jsonl = [
        json!({"jsonrpc": "2.0", "method": "session/update", "params": {"update": {
            "sessionUpdate": "plan", "entries": [
                {"content": "Edit", "status": "completed", "priority": "high"},
                {"content": "Test", "status": "in_progress", "priority": "low"}]}}}),
        json!({"jsonrpc": "2.0", "id": 3, "result": {"stopReason": "end_turn"}}),
    ]
    .map(|line| line.to_string())
    .join("\n");
    let events = replay("opencode", AcpStream::default(), &jsonl, Some(0), "");
    assert_eq!(
        events[0],
        SunEvent::Plan {
            todos: vec![
                Todo {
                    text: "Edit".into(),
                    status: TodoStatus::Done
                },
                Todo {
                    text: "Test".into(),
                    status: TodoStatus::InProgress
                },
            ]
        }
    );
}

// --- the live process path, with a fake `opencode acp` -------------------

/// Answers the handshake from the fixture's first two lines, then plays the
/// rest. Like the real CLI it ignores stdin EOF, with a child holding stdout.
const FAKE_OPENCODE: &str = r#"#!/bin/sh
case "$1" in
  --version) echo "1.18.34"; exit 0 ;;
esac
echo "$@" > "$SUN_OUT.args"
printf '%s\n' "$OPENCODE_CONFIG_CONTENT" > "$SUN_OUT.env"
read -r line; printf '%s\n' "$line" > "$SUN_OUT.stdin"
sed -n 1p "$SUN_FIXTURE"
read -r line; printf '%s\n' "$line" >> "$SUN_OUT.stdin"
sed -n 2p "$SUN_FIXTURE"
read -r line; printf '%s\n' "$line" >> "$SUN_OUT.stdin"
case "$SUN_MODE" in
  permission)
    echo '{"jsonrpc":"2.0","id":0,"method":"session/request_permission","params":{"options":[{"optionId":"a","kind":"allow_once"},{"optionId":"r","kind":"reject_once"}]}}'
    read -r line; printf '%s\n' "$line" >> "$SUN_OUT.stdin"
    echo '{"jsonrpc":"2.0","id":3,"result":{"stopReason":"end_turn"}}' ;;
  steer)
    read -r line; printf '%s\n' "$line" >> "$SUN_OUT.stdin"
    echo '{"jsonrpc":"2.0","id":3,"result":{"stopReason":"end_turn"}}'
    echo '{"jsonrpc":"2.0","id":4,"result":{"stopReason":"end_turn"}}' ;;
  hang)
    read -r line; printf '%s\n' "$line" >> "$SUN_OUT.stdin"
    echo '{"jsonrpc":"2.0","id":3,"result":{"stopReason":"cancelled"}}' ;;
  *) sed -n '3,$p' "$SUN_FIXTURE" ;;
esac
cat > /dev/null
sleep 300
"#;

fn normal_fixture() -> String {
    format!("{}/normal.jsonl", common::pinned_dir("opencode"))
}

fn stdin(dir: &std::path::Path) -> Vec<Value> {
    std::fs::read_to_string(dir.join("out.stdin"))
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

#[tokio::test]
async fn live_run_does_the_handshake_and_matches_replay() {
    let dir = sandbox("opencode", FAKE_OPENCODE, "live");
    let adapter = OpenCode::new(dir.join("opencode"));
    let fixture_path = normal_fixture();
    let mut spec = spec(&dir, &[("SUN_FIXTURE", &fixture_path)]);
    spec.model = Some("opencode/big-pickle".into());
    let started = std::time::Instant::now();
    let handle = adapter.start(spec).await.unwrap();
    assert_eq!(
        handle.native_id.as_deref(),
        Some("ses_eed7ab296ffe5VT5bK6KUkidwF")
    );
    let events = collect(adapter.events(&handle)).await;
    // Killed once its turn ended, not `process::GRACE` (10 s) later.
    assert!(started.elapsed().as_secs() < 5, "{:?}", started.elapsed());
    let (jsonl, _, _) = fixture("opencode", "normal");
    let normalizer = AcpStream::new(String::new(), Some(dir.clone()), None);
    assert_eq!(events, replay("opencode", normalizer, &jsonl, None, ""));

    let sent = stdin(&dir);
    assert_eq!(sent[0]["method"], "initialize");
    assert_eq!(sent[0]["params"]["protocolVersion"], 1);
    assert_eq!(sent[1]["method"], "session/new");
    assert_eq!(sent[1]["params"]["cwd"], dir.display().to_string());
    assert_eq!(sent[2]["method"], "session/prompt");
    assert_eq!(sent[2]["id"], 3);
    assert_eq!(
        sent[2]["params"]["sessionId"],
        "ses_eed7ab296ffe5VT5bK6KUkidwF"
    );
    assert_eq!(sent[2]["params"]["prompt"][0]["text"], "Create hello.txt");
    assert_eq!(
        std::fs::read_to_string(dir.join("out.args")).unwrap(),
        "acp\n"
    );
    assert_eq!(
        std::fs::read_to_string(dir.join("out.env")).unwrap(),
        "{\"model\":\"opencode/big-pickle\",\"permission\":{\"bash\":\"ask\",\"edit\":\"ask\",\"webfetch\":\"ask\"}}\n"
    );
}

#[tokio::test]
async fn permission_requests_are_rejected() {
    let dir = sandbox("opencode", FAKE_OPENCODE, "permission");
    let adapter = OpenCode::new(dir.join("opencode"));
    let fixture_path = normal_fixture();
    let spec = spec(
        &dir,
        &[("SUN_FIXTURE", &fixture_path), ("SUN_MODE", "permission")],
    );
    let handle = adapter.start(spec).await.unwrap();
    collect(adapter.events(&handle)).await;
    assert_eq!(
        stdin(&dir)[3],
        json!({"jsonrpc": "2.0", "id": 0, "result": {"outcome": {"outcome": "selected", "optionId": "r"}}})
    );
}

#[tokio::test]
async fn steer_is_a_new_prompt_in_the_same_session() {
    let dir = sandbox("opencode", FAKE_OPENCODE, "steer");
    let adapter = OpenCode::new(dir.join("opencode"));
    let fixture_path = normal_fixture();
    let spec = spec(
        &dir,
        &[("SUN_FIXTURE", &fixture_path), ("SUN_MODE", "steer")],
    );
    let handle = adapter.start(spec).await.unwrap();
    let steer = Steer {
        text: "use snake_case".into(),
    };
    adapter.send(&handle, steer).await.unwrap();
    let done = SunEvent::DoneClaimed {
        summary: String::new(),
    };
    assert_eq!(
        collect(adapter.events(&handle)).await,
        [done.clone(), done, SunEvent::Exited { code: None }]
    );
    let sent = stdin(&dir);
    assert_eq!(sent[3]["id"], 4);
    assert_eq!(
        sent[3]["params"]["sessionId"],
        "ses_eed7ab296ffe5VT5bK6KUkidwF"
    );
    assert_eq!(sent[3]["params"]["prompt"][0]["text"], "use snake_case");
}

#[tokio::test]
async fn graceful_stop_cancels_the_turn_and_ends_promptly() {
    let dir = sandbox("opencode", FAKE_OPENCODE, "cancel");
    let adapter = OpenCode::new(dir.join("opencode"));
    let fixture_path = normal_fixture();
    let spec = spec(
        &dir,
        &[("SUN_FIXTURE", &fixture_path), ("SUN_MODE", "hang")],
    );
    let handle = adapter.start(spec).await.unwrap();
    let started = std::time::Instant::now();
    adapter.stop(&handle, StopMode::Graceful).await.unwrap();
    assert_eq!(
        collect(adapter.events(&handle)).await,
        [SunEvent::Exited { code: None }]
    );
    assert!(started.elapsed().as_secs() < 5, "{:?}", started.elapsed());
    assert_eq!(
        stdin(&dir)[3],
        json!({"jsonrpc": "2.0", "method": "session/cancel", "params": {"sessionId": "ses_eed7ab296ffe5VT5bK6KUkidwF"}})
    );
}

#[tokio::test]
async fn readiness_needs_no_login() {
    let dir = sandbox("opencode", FAKE_OPENCODE, "ready");
    let host = HostCtx {
        host_id: HostId::generate(),
    };
    let ready = OpenCode::new(dir.join("opencode")).readiness(&host).await;
    assert!(ready.installed && ready.logged_in, "{ready:?}");
    assert_eq!(ready.version.as_deref(), Some("1.18.34"));
    assert_eq!(ready.detail, None);
    let missing = OpenCode::new(dir.join("not-there")).readiness(&host).await;
    assert!(!missing.installed);
    assert!(
        missing
            .detail
            .unwrap()
            .contains("npm install -g opencode-ai")
    );
}

/// OpenCode's effort is a per-provider `--variant` that Sun doesn't pass
/// yet: the probe offers none, so the picker greys out.
#[tokio::test]
async fn effort_offers_none() {
    let dir = sandbox("opencode", FAKE_OPENCODE, "effort");
    let host = HostCtx {
        host_id: HostId::generate(),
    };
    let catalog = OpenCode::new(dir.join("opencode")).effort(&host).await;
    assert_eq!(catalog, sun_core::EffortCatalog::default());
}

/// Loads an old session: replays its history, then answers the load.
const FAKE_OPENCODE_LOAD: &str = r#"#!/bin/sh
[ "$1" = --version ] && exit 0
read -r line; printf '%s\n' "$line" > "$SUN_OUT.stdin"
echo '{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":1}}'
read -r line; printf '%s\n' "$line" >> "$SUN_OUT.stdin"
echo '{"jsonrpc":"2.0","method":"session/update","params":{"update":{"sessionUpdate":"agent_message_chunk","messageId":"m1","content":{"text":"old history"}}}}'
echo '{"jsonrpc":"2.0","id":2,"result":null}'
read -r line; printf '%s\n' "$line" >> "$SUN_OUT.stdin"
echo '{"jsonrpc":"2.0","method":"session/update","params":{"update":{"sessionUpdate":"agent_message_chunk","messageId":"m2","content":{"text":"resumed"}}}}'
echo '{"jsonrpc":"2.0","id":3,"result":{"stopReason":"end_turn"}}'
cat > /dev/null
sleep 300
"#;

#[tokio::test]
async fn resume_loads_the_session_without_replaying_its_history() {
    let dir = sandbox("opencode", FAKE_OPENCODE_LOAD, "resume");
    let adapter = OpenCode::new(dir.join("opencode"));
    let mut spec = spec(&dir, &[]);
    spec.resume = Some("ses_old".into());
    let handle = adapter.start(spec).await.unwrap();
    assert_eq!(handle.native_id.as_deref(), Some("ses_old"));
    let events = collect(adapter.events(&handle)).await;
    assert_eq!(
        events,
        [
            SunEvent::Text {
                role: Role::Assistant,
                text: "resumed".into()
            },
            SunEvent::DoneClaimed {
                summary: "resumed".into()
            },
            SunEvent::Exited { code: None },
        ]
    );
    let sent = stdin(&dir);
    assert_eq!(sent[1]["method"], "session/load");
    assert_eq!(sent[1]["params"]["sessionId"], "ses_old");
    assert_eq!(sent[2]["method"], "session/prompt");
    assert_eq!(sent[2]["params"]["sessionId"], "ses_old");
}

#[tokio::test]
async fn builtin_template_reaches_the_first_prompt() {
    let dir = sandbox("opencode", FAKE_OPENCODE, "template");
    let adapter = OpenCode::new(dir.join("opencode"));
    let fixture_path = normal_fixture();
    let mut spec = spec(&dir, &[("SUN_FIXTURE", &fixture_path)]);
    spec.envelope_parts = Some(parts());
    let handle = adapter.start(spec).await.unwrap();
    collect(adapter.events(&handle)).await;
    let prompt = stdin(&dir)[2]["params"]["prompt"][0]["text"].clone();
    let prompt = prompt.as_str().unwrap();
    assert!(
        prompt.starts_with("# Task\n\nCreate hello.txt\n"),
        "{prompt}"
    );
    assert!(prompt.contains("- cargo test\n"), "{prompt}");
    assert!(prompt.contains("(todowrite)"), "{prompt}");
}

/// BLO-163: OpenCode has no reviewer for `auto`, so its host reports only
/// the four modes it enforces exactly.
#[tokio::test]
async fn opencode_reports_every_mode_but_auto() {
    let host = HostCtx {
        host_id: HostId::generate(),
    };
    let modes = OpenCode::new("opencode").permission_modes(&host).await;
    assert_eq!(modes, sun_core::PermissionMode::LEGACY);
}

/// BLO-163: OpenCode allows every tool by default, so each session's config
/// makes it ask (Sun answers by mode), in every mode and merged over a
/// provider config the session env already carries (Sun credits).
#[tokio::test]
async fn every_mode_makes_opencode_ask() {
    let ask = json!({"bash": "ask", "edit": "ask", "webfetch": "ask"});
    for mode in sun_core::PERMISSION_MODES {
        let dir = sandbox("opencode", FAKE_OPENCODE, &format!("mode-{mode}"));
        let adapter = OpenCode::new(dir.join("opencode"));
        let provider = r#"{"provider":{"sun":{}},"model":"sun/m"}"#;
        let fixture_path = normal_fixture();
        let env = [
            ("SUN_FIXTURE", fixture_path.as_str()),
            ("OPENCODE_CONFIG_CONTENT", provider),
        ];
        let mut spec = spec(&dir, &env);
        spec.permission_mode = Some(mode.into());
        let handle = adapter.start(spec).await.unwrap();
        collect(adapter.events(&handle)).await;
        let config: Value =
            serde_json::from_str(&std::fs::read_to_string(dir.join("out.env")).unwrap()).unwrap();
        assert_eq!(config["permission"], ask, "{mode}");
        assert_eq!(
            config["model"], "sun/m",
            "{mode}: the provider config is kept"
        );
    }
}
