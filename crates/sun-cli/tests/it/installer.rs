//! Installing mods from the UI (`KernelInstaller`): a mod of the registry
//! installed from its GitHub Release asset (served by an in-test server) and
//! loaded into the running mod host; consent, a missing asset and unknown
//! names refuse the rest.
#![allow(clippy::unwrap_used)]

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

use axum::Router;
use axum::routing::get;
use serde_json::{Value, json};
use sun_cli::installer::KernelInstaller;
use sun_cli::registry::{Entry, TARGET};
use sun_core::model::ModKind;
use sun_core::{Error, ModCall, ModInstaller, ModLink};
use sun_store::{AuditRepo, ModInstallRepo, Store};

/// An unsigned package of the fixture mod named `name`.
fn package(dir: &Path, name: &str, version: &str, secret: &str) -> PathBuf {
    let package = dir.join(name);
    std::fs::create_dir_all(&package).unwrap();
    std::fs::write(
        package.join("mod.yaml"),
        format!(
            "name: {name}\nkind: plugin\nversion: {version}\napi: \">=1.0 <2.0\"\n\
             entry: {{ command: {:?} }}\nprovides: {{ actions: [config] }}\n\
             permissions: {{ secrets: [{secret}] }}\n",
            env!("CARGO_BIN_EXE_sun-e2e-mod")
        ),
    )
    .unwrap();
    package
}

fn entry(name: &str, tier: &str, repo: String) -> Entry {
    Entry {
        name: name.into(),
        kind: ModKind::Plugin,
        version: "0.2.0".into(),
        description: format!("The {name} mod."),
        repo,
        tier: tier.into(),
        permissions: vec!["secret:MARKET_TOKEN".into()],
    }
}

async fn config_of(host: &sun_mod::ModHost, name: &str) -> Value {
    let call = ModCall {
        mod_name: name.into(),
        method: "config".into(),
        params: Value::Null,
        idem_key: None,
    };
    ModLink::call(host, call).await.unwrap()
}

#[tokio::test]
async fn ui_installs_from_the_registry_release_and_loads_it() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("data");
    std::fs::create_dir_all(&data).unwrap();
    std::fs::write(
        data.join("sun.yaml"),
        "mods: {market: {config: {api_url: \"https://m.example\"}}}\n",
    )
    .unwrap();

    // The release asset, cargo-dist style: one `mod-<name>-<target>/` folder.
    let published = dir.path().join("published");
    let folder = format!("mod-market-{TARGET}");
    let market = package(&published, "market", "0.2.0", "MARKET_TOKEN");
    std::fs::rename(&market, published.join(&folder)).unwrap();
    let asset = dir.path().join("asset.tar.gz");
    let tar = Command::new("tar")
        .arg("-czf")
        .arg(&asset)
        .arg("-C")
        .arg(&published)
        .arg(&folder)
        .status()
        .unwrap();
    assert!(tar.success());
    let asset = std::fs::read(&asset).unwrap();
    let route = format!("/madlabs-tech/sun-plugin-market/releases/download/v0.2.0/{folder}.tar.gz");
    let app = Router::new().route(&route, get(move || async move { asset.clone() }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let server = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await });

    let store = Store::open(&data).await.unwrap();
    let host = Arc::new(sun_mod::ModHost::new(sun_mod::HostConfig::default()));
    let installer = KernelInstaller {
        store: store.clone(),
        data_dir: data.clone(),
        host: host.clone(),
        sandbox: false,
        trusted_keys: vec![],
        registry: vec![
            entry(
                "market",
                "official",
                format!("{server}/madlabs-tech/sun-plugin-market"),
            ),
            entry(
                "nope",
                "community",
                format!("{server}/someone/sun-plugin-nope"),
            ),
        ],
        engine: None,
        loaded: Default::default(),
    };

    let available = installer.available().await.unwrap();
    let summary: Vec<_> = available
        .iter()
        .map(|m| (m.name.as_str(), m.tier.as_str(), m.repo.as_str(), m.signed))
        .collect();
    assert_eq!(
        summary,
        [
            (
                "market",
                "official",
                &*format!("{server}/madlabs-tech/sun-plugin-market"),
                false
            ),
            (
                "nope",
                "community",
                &*format!("{server}/someone/sun-plugin-nope"),
                false
            ),
        ]
    );

    // Consent must match exactly what is listed.
    let err = installer
        .install("market", &[], "user:me")
        .await
        .unwrap_err();
    assert!(matches!(err, Error::InvalidInput(_)), "{err}");
    assert!(err.to_string().contains("secret:MARKET_TOKEN"), "{err}");

    let row = installer
        .install("market", &["secret:MARKET_TOKEN".into()], "user:me")
        .await
        .unwrap();
    assert_eq!(row.version, "0.2.0");
    assert_eq!(row.signed_by, None);
    // Loaded at once, with its sun.yaml install config.
    assert_eq!(
        config_of(&host, "market").await,
        json!({ "api_url": "https://m.example" })
    );
    let staged = std::fs::read_dir(data.join("mods")).unwrap().flatten();
    assert!(
        staged
            .map(|e| e.file_name())
            .all(|n| !n.to_string_lossy().starts_with(".download")),
        "staging left behind"
    );

    // Listed, but no release asset for this platform.
    let err = installer
        .install("nope", &["secret:MARKET_TOKEN".into()], "user:me")
        .await
        .unwrap_err();
    assert!(matches!(err, Error::Unavailable(_)), "{err}");
    assert!(err.to_string().contains("no release asset"), "{err}");
    assert!(!data.join("mods/nope").exists());
    let err = installer
        .install("missing", &[], "user:me")
        .await
        .unwrap_err();
    assert!(matches!(err, Error::NotFound(_)), "{err}");

    let mut tx = store.read().await.unwrap();
    let names: Vec<_> = tx
        .mod_installs()
        .await
        .unwrap()
        .into_iter()
        .map(|m| m.name)
        .collect();
    assert_eq!(names, ["market"]);
    let audit = tx.audit_log(None, 10).await.unwrap();
    assert!(
        audit
            .iter()
            .any(|a| a.actor == "user:me" && a.target == "market"),
        "{audit:?}"
    );
    drop(tx);

    // A CLI removal reaches the running host on the next sync, no restart.
    installer.seed().await.unwrap();
    sun_cli::mods::remove(&data, "market", false).await.unwrap();
    assert!(host.health("market").await.is_ok());
    installer.sync().await.unwrap();
    let err = host.health("market").await.unwrap_err();
    assert!(matches!(err, Error::NotFound(_)), "{err}");

    // The UI removes at once; its data goes only with purge.
    installer
        .install("market", &["secret:MARKET_TOKEN".into()], "user:me")
        .await
        .unwrap();
    assert!(
        installer
            .remove("market", true, "user:me")
            .await
            .unwrap()
            .is_empty()
    );
    assert!(matches!(
        host.health("market").await,
        Err(Error::NotFound(_))
    ));
    assert!(!data.join("mods/market").exists());
    let err = installer.remove("market", false, "user:me").await;
    assert!(matches!(err, Err(Error::NotFound(_))), "{err:?}");
    host.shutdown().await;
}
