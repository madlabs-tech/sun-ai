//! Extensions reach third parties only through the plugins they declare
//! (`plugin_call`, API 1.1), and deprecated methods warn but keep working.
//! All mods are the echo test mod.
#![allow(clippy::unwrap_used, clippy::expect_used)] // test helpers

use std::time::Duration;

use serde_json::{Value, json};
use sun_core::{Error, ModCall, ModLink};
use sun_mod::{HostConfig, ModHost, parse_manifest};
use sun_mod_sdk::Deprecation;

async fn add(host: &ModHost, name: &str, kind: &str, provides: &str, plugins: &str) {
    let yaml = format!(
        "name: {name}\nkind: {kind}\nversion: 0.1.0\napi: \">=1.0 <2.0\"\n\
         entry: {{ command: \"{}\" }}\nprovides: {provides}\n\
         permissions: {{ plugins: [{plugins}] }}\n",
        env!("CARGO_BIN_EXE_sun-echo-mod")
    );
    host.add(parse_manifest(&yaml).unwrap(), std::env::temp_dir())
        .await
        .unwrap();
}

/// `notifier` and `other` are plugins; extension `relay` declares `notifier`;
/// plugin `sneaky` declares it too, but plugins may not call plugins.
async fn host(config: HostConfig) -> ModHost {
    let host = ModHost::new(config);
    add(&host, "notifier", "plugin", "{ actions: [echo] }", "").await;
    add(&host, "other", "plugin", "{ actions: [echo] }", "").await;
    add(
        &host,
        "relay",
        "extension",
        "{ step_types: [relay] }",
        "notifier",
    )
    .await;
    add(
        &host,
        "sneaky",
        "plugin",
        "{ actions: [relay] }",
        "notifier",
    )
    .await;
    host
}

async fn relay(host: &ModHost, from: &str, plugin: &str) -> sun_core::Result<Value> {
    host.call(ModCall {
        mod_name: from.into(),
        method: "relay".into(),
        params: json!({ "plugin": plugin, "action": "echo", "params": { "x": 1 } }),
        idem_key: None,
    })
    .await
}

#[tokio::test(flavor = "multi_thread")]
async fn extension_reaches_only_declared_plugins() {
    let host = host(HostConfig::default()).await;
    let out = relay(&host, "relay", "notifier").await.unwrap();
    assert_eq!(out["params"], json!({ "x": 1 }));

    for (from, plugin) in [("relay", "other"), ("sneaky", "notifier")] {
        let err = relay(&host, from, plugin).await.unwrap_err();
        assert!(
            matches!(&err, Error::InvalidInput(m) if m.contains("permissions.plugins")),
            "{from} → {plugin}: {err}"
        );
    }
    assert!(
        !host.is_running("other").await,
        "a refused call must not reach the plugin"
    );
    host.shutdown().await;
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
async fn deprecated_call_warns() {
    let captured = Captured::default();
    let writer = captured.clone();
    tracing::subscriber::set_global_default(
        tracing_subscriber::fmt()
            .with_writer(move || writer.clone())
            .with_ansi(false)
            .finish(),
    )
    .unwrap();
    const OLD: &[Deprecation] = &[Deprecation {
        method: "plugin_call",
        since: "1.1.0",
        use_instead: "plugin_call_v2",
    }];
    let host = host(HostConfig {
        deprecated: OLD,
        ..HostConfig::default()
    })
    .await;

    // Still works for at least one minor release...
    let out = relay(&host, "relay", "notifier").await.unwrap();
    assert_eq!(out["params"], json!({ "x": 1 }));
    // ...with a warning naming the mod, the version and the replacement.
    let mut log = String::new();
    for _ in 0..100 {
        log = String::from_utf8_lossy(&captured.0.lock().unwrap()).into_owned();
        if log.contains("deprecated") {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(
        log.contains("WARN")
            && log.contains(
                "mod relay: plugin_call is deprecated since mod API 1.1.0; use plugin_call_v2"
            ),
        "{log}"
    );
    host.shutdown().await;
}
