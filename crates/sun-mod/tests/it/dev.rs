//! `sun mod dev`: editing a mod's folder hot-reloads it.
#![allow(clippy::unwrap_used, clippy::expect_used)] // test helpers

use std::sync::Arc;
use std::time::Duration;

use serde_json::json;
use sun_core::{Error, ModCall, ModLink};
use sun_mod::{HostConfig, ModHost, watch_dev};
use tokio::sync::mpsc;

fn manifest(actions: &str) -> String {
    format!(
        "name: echo\nkind: plugin\nversion: 0.1.0\napi: '>=1.0 <2.0'\n\
         entry: {{ command: ./echo }}\nprovides: {{ actions: [{actions}] }}\n"
    )
}

#[tokio::test(flavor = "multi_thread")]
async fn dev_reloads_the_mod_when_its_folder_changes() {
    let dir = std::env::temp_dir().join(format!("sun-dev-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::copy(env!("CARGO_BIN_EXE_sun-echo-mod"), dir.join("echo")).unwrap();
    crate::warm(&dir.join("echo"));
    std::fs::write(dir.join("mod.yaml"), manifest("echo")).unwrap();

    let host = Arc::new(ModHost::new(HostConfig::default()));
    let (tx, mut reloads) = mpsc::unbounded_channel();
    let watcher = tokio::spawn({
        let (host, dir) = (host.clone(), dir.clone());
        async move {
            let poll = Duration::from_millis(50);
            watch_dev(&host, &dir, None, poll, |r| drop(tx.send(r))).await;
        }
    });
    let mut next = async || {
        tokio::time::timeout(Duration::from_secs(5), reloads.recv())
            .await
            .expect("no reload within 5s")
            .unwrap()
    };
    assert_eq!(next().await.unwrap(), "echo");
    let call = |action: &str| ModCall {
        mod_name: "echo".into(),
        method: action.into(),
        params: json!({}),
        idem_key: None,
    };
    host.call(call("echo")).await.unwrap();
    let undeclared = host.call(call("fail")).await.unwrap_err();
    assert!(undeclared.to_string().contains("does not provide"));

    // Declaring `fail` reloads the mod: the call now reaches it.
    std::fs::write(dir.join("mod.yaml"), manifest("echo, fail")).unwrap();
    assert_eq!(next().await.unwrap(), "echo");
    match host.call(call("fail")).await {
        Err(Error::InvalidInput(msg)) => assert!(msg.contains("boom"), "{msg}"),
        other => panic!("{other:?}"),
    }

    // A broken mod.yaml is reported; the last good version keeps serving.
    std::fs::write(dir.join("mod.yaml"), "name: [").unwrap();
    assert!(next().await.is_err());
    host.call(call("echo")).await.unwrap();

    watcher.abort();
    host.shutdown().await;
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn templates_have_valid_manifests() {
    let templates = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../templates");
    for name in ["plugin-typescript", "extension-rust"] {
        sun_mod::load_manifest(&templates.join(name)).unwrap_or_else(|e| panic!("{name}: {e}"));
    }
}
