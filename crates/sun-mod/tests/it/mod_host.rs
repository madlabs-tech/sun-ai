//! End-to-end: the host against the real echo test mod (`tests/fixtures/echo_mod.rs`).
#![allow(clippy::unwrap_used, clippy::expect_used)] // test helpers

use std::path::PathBuf;
use std::pin::Pin;
use std::time::Duration;

use futures_core::Stream;
use serde_json::{Value, json};
use sun_core::{BoxStream, Error, IdemKey, ModCall, ModEvent, ModLink, RunId, StepKey};
use sun_mod::{HostConfig, ModHost, parse_manifest};

fn config(idle_ms: u64) -> HostConfig {
    HostConfig {
        call_timeout: Duration::from_millis(1500),
        idle_timeout: Duration::from_millis(idle_ms),
        backoff_base: Duration::from_millis(50),
        backoff_max: Duration::from_millis(400),
        shutdown_grace: Duration::from_millis(500),
        ..HostConfig::default()
    }
}

async fn host(idle_ms: u64) -> ModHost {
    let yaml = format!(
        r#"
name: echo
kind: plugin
version: 0.1.0
api: ">=1.0 <2.0"
entry: {{ command: "{}" }}
provides:
  actions: [echo, log, fail, crash, hang]
  triggers: [tick]
"#,
        env!("CARGO_BIN_EXE_sun-echo-mod")
    );
    let host = ModHost::new(config(idle_ms));
    host.add(parse_manifest(&yaml).unwrap(), std::env::temp_dir())
        .await
        .unwrap();
    host
}

fn call(method: &str, params: Value) -> ModCall {
    ModCall {
        mod_name: "echo".into(),
        method: method.into(),
        params,
        idem_key: None,
    }
}

async fn next(events: &mut BoxStream<'static, ModEvent>) -> ModEvent {
    let poll = std::future::poll_fn(|cx| Pin::new(&mut *events).poll_next(cx));
    tokio::time::timeout(Duration::from_secs(5), poll)
        .await
        .expect("no event within 5s")
        .expect("event stream ended")
}

#[tokio::test(flavor = "multi_thread")]
async fn starts_lazily_and_round_trips_a_call() {
    let host = host(60_000).await;
    assert!(!host.is_running("echo").await);

    let mut echo = call("echo", json!({ "x": 1 }));
    let run = RunId::generate();
    echo.idem_key = Some(IdemKey::new(run, &StepKey::new("build").unwrap(), "echo"));
    let out = host.call(echo).await.unwrap();
    assert_eq!(out["params"], json!({ "x": 1 }));
    assert_eq!(out["idem_key"], json!(format!("{run}:build:echo")));
    assert!(host.is_running("echo").await);
    assert_eq!(host.health("echo").await.unwrap(), json!({ "ok": true }));

    host.shutdown().await;
    assert!(!host.is_running("echo").await);
}

#[tokio::test(flavor = "multi_thread")]
async fn rejects_undeclared_actions_and_maps_mod_errors() {
    let host = host(60_000).await;
    let err = host.call(call("nope", Value::Null)).await.unwrap_err();
    assert!(matches!(err, Error::InvalidInput(_)), "{err}");
    assert!(
        !host.is_running("echo").await,
        "validation must not start the mod"
    );

    let err = host.call(call("fail", Value::Null)).await.unwrap_err();
    assert_eq!(err, Error::InvalidInput("mod echo: boom".into()));

    let missing = ModCall {
        mod_name: "ghost".into(),
        ..call("echo", Value::Null)
    };
    assert!(matches!(host.call(missing).await, Err(Error::NotFound(_))));
}

/// The engine runs a workflow step `type: echo.echo` as this call.
#[tokio::test(flavor = "multi_thread")]
async fn extension_step_types_are_callable() {
    let yaml = format!(
        "name: echo\nkind: extension\nversion: 0.1.0\napi: \">=1.0 <2.0\"\n\
         entry: {{ command: \"{}\" }}\nprovides: {{ step_types: [echo] }}\n",
        env!("CARGO_BIN_EXE_sun-echo-mod")
    );
    let host = ModHost::new(config(60_000));
    host.add(parse_manifest(&yaml).unwrap(), std::env::temp_dir())
        .await
        .unwrap();
    let mut step = call("echo", json!({ "with": { "url": "u" } }));
    let run = RunId::generate();
    step.idem_key = Some(IdemKey::new(
        run,
        &StepKey::new("shot").unwrap(),
        "echo.echo#1",
    ));
    let out = host.call(step).await.unwrap();
    assert_eq!(out["params"]["with"]["url"], "u");
    assert_eq!(out["idem_key"], json!(format!("{run}:shot:echo.echo#1")));
    let err = host.call(call("fail", Value::Null)).await.unwrap_err();
    assert!(matches!(err, Error::InvalidInput(_)), "{err}");
    host.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_crash_fails_only_that_call_and_the_next_call_restarts() {
    let host = host(60_000).await;
    let err = host.call(call("crash", Value::Null)).await.unwrap_err();
    assert!(matches!(err, Error::Unavailable(_)), "{err}");

    let out = host.call(call("echo", json!(2))).await.unwrap();
    assert_eq!(out["params"], json!(2));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_hang_times_out_and_the_mod_is_replaced() {
    let host = host(60_000).await;
    let err = host.call(call("hang", Value::Null)).await.unwrap_err();
    assert!(err.to_string().contains("timed out"), "{err}");
    let out = host.call(call("echo", json!(3))).await.unwrap();
    assert_eq!(out["params"], json!(3));
}

#[tokio::test(flavor = "multi_thread")]
async fn an_idle_mod_is_stopped() {
    let host = host(150).await;
    host.call(call("echo", Value::Null)).await.unwrap();
    assert!(host.is_running("echo").await);
    tokio::time::sleep(Duration::from_millis(800)).await;
    assert!(!host.is_running("echo").await);
}

#[tokio::test(flavor = "multi_thread")]
async fn subscriptions_deliver_events_and_survive_a_crash() {
    let host = host(150).await;
    let mut events = host.events();

    let err = host
        .subscribe("echo", vec!["nope".into()])
        .await
        .unwrap_err();
    assert!(matches!(err, Error::InvalidInput(_)));

    host.subscribe("echo", vec!["tick".into()]).await.unwrap();
    let first = next(&mut events).await;
    assert_eq!(
        (first.mod_name.as_str(), first.topic.as_str()),
        ("echo", "tick")
    );
    // Another feature subscribing adds to the set; it never drops `tick`.
    // (The echo mod emits each trigger it is sent, so `tick` comes again.)
    host.subscribe("echo", vec![]).await.unwrap();
    assert_eq!(next(&mut events).await.payload, first.payload);

    // Crash it: the supervisor restarts and re-subscribes without any call.
    let _ = host.call(call("crash", Value::Null)).await;
    let second = next(&mut events).await;
    assert_eq!(second.topic, "tick");
    assert_ne!(first.payload["pid"], second.payload["pid"]);

    // Subscribed mods are never idle-stopped.
    tokio::time::sleep(Duration::from_millis(600)).await;
    assert!(host.is_running("echo").await);

    host.remove("echo").await.unwrap();
    assert!(!host.is_running("echo").await);
}

#[test]
fn add_dir_reads_mod_yaml() {
    let dir: PathBuf = std::env::temp_dir().join(format!("sun-mod-test-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("mod.yaml"),
        "name: tiny\nkind: extension\nversion: 1.0.0\napi: ^1\nentry: { command: ./tiny }\n",
    )
    .unwrap();
    let rt = tokio::runtime::Runtime::new().unwrap();
    let name = rt.block_on(ModHost::new(HostConfig::default()).add_dir(&dir));
    std::fs::remove_dir_all(&dir).unwrap();
    assert_eq!(name.unwrap(), "tiny");
}

/// Collects what the `tracing` subscriber writes.
#[derive(Clone, Default)]
struct Captured(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

impl std::io::Write for Captured {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn mod_stderr_is_redacted() {
    let captured = Captured::default();
    let writer = captured.clone();
    tracing::subscriber::set_global_default(
        tracing_subscriber::fmt()
            .with_writer(move || writer.clone())
            .with_ansi(false)
            .finish(),
    )
    .unwrap();
    // Stored after boot, as `PUT /secrets` does: the store remembers it.
    sun_core::redact::remember_secret("hunter2-mod-secret");

    let host = host(60_000).await;
    let text = json!({ "text": "connecting with hunter2-mod-secret now" });
    host.call(call("log", text)).await.unwrap();
    let mut log = String::new();
    for _ in 0..100 {
        log = String::from_utf8_lossy(&captured.0.lock().unwrap()).into_owned();
        if log.contains("connecting with") {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(
        log.contains("mod echo: connecting with [REDACTED] now"),
        "{log}"
    );
    assert!(!log.contains("hunter2"), "{log}");
    host.shutdown().await;
}
