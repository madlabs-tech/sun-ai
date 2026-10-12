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

/// The cargo-dist asset of a fixture mod: `mod-<name>-<TARGET>/` holding a
/// package that says `version` and asks for `secret`, signed with `seed`.
fn release_asset(
    dir: &Path,
    name: &str,
    version: &str,
    secret: &str,
    seed: Option<&[u8; 32]>,
) -> Vec<u8> {
    let published = dir.join(format!("published-{name}"));
    let folder = format!("mod-{name}-{TARGET}");
    let package = package(&published, name, version, secret);
    if let Some(seed) = seed {
        sun_mod::sign_package(&package, seed).unwrap();
    }
    std::fs::rename(&package, published.join(&folder)).unwrap();
    let asset = dir.join(format!("{name}.tar.gz"));
    let tar = Command::new("tar")
        .arg("-czf")
        .arg(&asset)
        .arg("-C")
        .arg(&published)
        .arg(&folder)
        .status()
        .unwrap();
    assert!(tar.success());
    std::fs::read(&asset).unwrap()
}

/// A release asset that breaks its registry listing is refused and leaves
/// nothing behind: it asks for a permission the listing doesn't show, it is
/// another version, it is over the download limit, or it is signed by a key
/// the kernel doesn't trust. A trusted signer installs, with `signed_by`.
#[tokio::test]
async fn ui_install_refuses_a_release_that_breaks_its_listing() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("data");
    std::fs::create_dir_all(&data).unwrap();
    let seed = [7u8; 32];
    let assets = [
        (
            "perms",
            release_asset(dir.path(), "perms", "0.2.0", "OTHER_TOKEN", None),
        ),
        (
            "drift",
            release_asset(dir.path(), "drift", "0.3.0", "MARKET_TOKEN", None),
        ),
        // `installer::MAX_PACKAGE` is 33 MiB.
        ("huge", vec![0u8; 34 * 1024 * 1024]),
        (
            "signed",
            release_asset(dir.path(), "signed", "0.2.0", "MARKET_TOKEN", Some(&seed)),
        ),
    ];
    let mut app = Router::new();
    for (name, asset) in assets {
        let asset = Arc::new(asset);
        let route = format!("/acme/{name}/releases/download/v0.2.0/mod-{name}-{TARGET}.tar.gz");
        app = app.route(&route, get(move || async move { asset.to_vec() }));
    }
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let server = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await });

    let store = Store::open(&data).await.unwrap();
    let host = Arc::new(sun_mod::ModHost::new(sun_mod::HostConfig::default()));
    let registry = ["perms", "drift", "huge", "signed"]
        .map(|name| entry(name, "community", format!("{server}/acme/{name}")))
        .to_vec();
    let mut installer = KernelInstaller {
        store: store.clone(),
        data_dir: data.clone(),
        host: host.clone(),
        sandbox: false,
        trusted_keys: vec![],
        registry,
        engine: None,
        loaded: Default::default(),
    };
    let granted = ["secret:MARKET_TOKEN".to_string()];
    let refused = |err: Error, kind: &str, says: &str| {
        let ok = match kind {
            "invalid" => matches!(err, Error::InvalidInput(_)),
            _ => matches!(err, Error::Unavailable(_)),
        };
        assert!(
            ok && err.to_string().contains(says),
            "{kind} {says:?}: {err}"
        );
    };

    let err = installer
        .install("perms", &granted, "user:me")
        .await
        .unwrap_err();
    refused(
        err,
        "invalid",
        "permission secret:OTHER_TOKEN was not approved",
    );
    let err = installer
        .install("drift", &granted, "user:me")
        .await
        .unwrap_err();
    refused(
        err,
        "invalid",
        "drift: the release v0.2.0 contains drift 0.3.0",
    );
    let err = installer
        .install("huge", &granted, "user:me")
        .await
        .unwrap_err();
    refused(err, "unavailable", "downloading huge 0.2.0");
    let err = installer
        .install("signed", &granted, "user:me")
        .await
        .unwrap_err();
    refused(err, "invalid", "signed by an untrusted key");

    let mods = data.join("mods");
    let left: Vec<_> = std::fs::read_dir(&mods)
        .map(|d| d.flatten().map(|e| e.file_name()).collect())
        .unwrap_or_default();
    assert!(left.is_empty(), "left behind: {left:?}");
    let rows = store.read().await.unwrap().mod_installs().await.unwrap();
    assert!(rows.is_empty(), "{rows:?}");

    // SUN_TRUSTED_KEYS names the signer: installed, and the row says by whom.
    installer.trusted_keys = vec![sun_mod::public_key(&seed)];
    let row = installer
        .install("signed", &granted, "user:me")
        .await
        .unwrap();
    let signer: String = sun_mod::public_key(&seed)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    assert_eq!(row.signed_by, Some(signer));
    assert!(mods.join("signed/mod.sig").is_file());
    host.shutdown().await;
}

/// `sun plugin add <name>` with a bare name the registry doesn't list says
/// what a source can be.
#[tokio::test]
async fn cli_add_of_a_name_not_in_the_registry_says_what_a_source_is() {
    let dir = tempfile::tempdir().unwrap();
    let err = sun_cli::mods::install(
        dir.path(),
        "no-such-mod-s8",
        sun_mod::ModKind::Plugin,
        &[],
        true,
        false,
    )
    .await
    .unwrap_err();
    assert_eq!(
        err.to_string(),
        "no-such-mod-s8: no such mod folder, package file, git+https:// URL or a name from the mod registry"
    );
    assert!(!dir.path().join("mods").exists());
}
