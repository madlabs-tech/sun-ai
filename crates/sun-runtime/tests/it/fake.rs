//! The fake runtime runs scripts through the real process driver and honors
//! the adapter contract, like every real CLI.
#![allow(clippy::unwrap_used)]

use crate::common;

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use common::{collect, spec};
use sun_core::{HostCtx, HostId, LimitKind, Role, RuntimeAdapter, Steer, StopMode, SunEvent};
use sun_runtime::FakeRuntime;
use sun_runtime::contract::{Case, violations};
use sun_runtime::fake::parse_script;

/// A fake runtime on `script`, written into a fresh dir.
fn fake(name: &str, script: &str) -> (FakeRuntime, PathBuf) {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("fake-{name}"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("script.jsonl"), script).unwrap();
    let adapter = FakeRuntime::new(
        format!("fake-{name}"),
        env!("CARGO_BIN_EXE_sun-fake-runtime"),
        dir.join("script.jsonl"),
    );
    (adapter, dir)
}

fn scripted(script: &str) -> Vec<SunEvent> {
    parse_script(script)
        .unwrap()
        .into_iter()
        .map(|line| line.event)
        .collect()
}

const NORMAL: &str = r#"
{"kind":"text","role":"assistant","text":"on it, key sk-live-123"}
{"kind":"tool_call","name":"Bash","args_hash":7,"summary":"cargo test"}
{"delay_ms":300,"kind":"tool_result","ok":true,"summary":"ok"}
{"kind":"file_changed","path":"src/lib.rs"}
{"kind":"usage","input":10,"output":5,"cost_usd":0.01,"context_used":null}
{"kind":"done_claimed","summary":"done"}
"#;

#[tokio::test]
async fn fake_replays_script_with_delays_redaction_and_contract() {
    let (adapter, dir) = fake("normal", NORMAL);
    let ready = adapter
        .readiness(&HostCtx {
            host_id: HostId::generate(),
        })
        .await;
    assert!(ready.installed && ready.logged_in, "{ready:?}");

    let began = Instant::now();
    let handle = adapter
        .start(spec(&dir, &[("API_KEY", "sk-live-123")]))
        .await
        .unwrap();
    assert!(handle.pid.is_some());
    let events = collect(adapter.events(&handle)).await;
    assert!(began.elapsed() >= Duration::from_millis(300));

    let mut expected = scripted(NORMAL);
    expected[0] = SunEvent::Text {
        role: Role::Assistant,
        text: "on it, key [REDACTED]".into(),
    };
    expected.push(SunEvent::Exited { code: Some(0) });
    assert_eq!(events, expected);
    assert_eq!(violations(Case::Normal, &events), Vec::<String>::new());
    assert_eq!(adapter.export(&handle).await.unwrap().events, events);
}

#[tokio::test]
async fn fake_limit_hit_and_crash_follow_the_contract() {
    let limit = r#"{"kind":"limit_hit","limit":"quota","reset_at":"2026-10-07T23:50:00Z","raw":"usage limit reached"}"#;
    let (adapter, dir) = fake("limit", limit);
    let handle = adapter.start(spec(&dir, &[])).await.unwrap();
    let events = collect(adapter.events(&handle)).await;
    assert!(matches!(
        events[0],
        SunEvent::LimitHit {
            limit: LimitKind::Quota,
            ..
        }
    ));
    assert_eq!(violations(Case::LimitHit, &events), Vec::<String>::new());

    let crash = r#"
{"kind":"tool_call","name":"Bash","args_hash":1,"summary":"make"}
{"kind":"exited","code":3}
"#;
    let (adapter, dir) = fake("crash", crash);
    let handle = adapter.start(spec(&dir, &[])).await.unwrap();
    let events = collect(adapter.events(&handle)).await;
    assert_eq!(events.last(), Some(&SunEvent::Exited { code: Some(3) }));
    assert_eq!(violations(Case::Crash, &events), Vec::<String>::new());
}

#[tokio::test]
async fn fake_stall_echoes_steers_and_dies_on_kill() {
    let stall = r#"
{"kind":"text","role":"assistant","text":"thinking"}
{"delay_ms":60000,"kind":"done_claimed","summary":"never"}
"#;
    let (adapter, dir) = fake("stall", stall);
    let handle = adapter.start(spec(&dir, &[])).await.unwrap();
    let mut stream = adapter.events(&handle);
    adapter
        .send(
            &handle,
            Steer {
                text: "two\nlines".into(),
            },
        )
        .await
        .unwrap();
    let mut seen = Vec::new();
    while seen.len() < 2 {
        let next = std::future::poll_fn(|cx| stream.as_mut().poll_next(cx));
        seen.push(
            tokio::time::timeout(Duration::from_secs(10), next)
                .await
                .unwrap()
                .unwrap(),
        );
    }
    assert!(seen.contains(&SunEvent::Text {
        role: Role::User,
        text: "two\nlines".into()
    }));

    let began = Instant::now();
    adapter.stop(&handle, StopMode::Kill).await.unwrap();
    adapter.stop(&handle, StopMode::Kill).await.unwrap();
    let rest = collect(stream).await;
    assert!(began.elapsed() < Duration::from_secs(5));
    assert!(
        matches!(rest.as_slice(), [SunEvent::Exited { .. }]),
        "{rest:?}"
    );
}

#[tokio::test]
async fn fake_bad_script_is_not_ready() {
    let (adapter, _) = fake("bad", "{\"kind\":\"teleport\"}");
    let ready = adapter
        .readiness(&HostCtx {
            host_id: HostId::generate(),
        })
        .await;
    assert!(!ready.installed);
    assert!(ready.detail.unwrap().contains("line 1"));
}
