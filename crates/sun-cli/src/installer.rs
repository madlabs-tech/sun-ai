//! Installing, updating and removing mods from the UI (`POST|PUT|DELETE
//! /api/v1/mods/{name}`): the mods of the embedded registry
//! ([`crate::registry`]), downloaded from their GitHub Release and applied to
//! the running mod host at once. [`install_archive`] is the verified install
//! of a marketplace package (`sun market install`).
//! [`KernelInstaller::sync`] applies what the `sun plugin|extension` CLI
//! wrote to the store, so CLI changes need no restart either.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use sun_core::model::ModInstall;
use sun_core::{AvailableMod, Error, ModInstaller, Result};
use sun_mod::ModHost;
use sun_store::{ModInstallRepo, Store};

use crate::config::Config;
use crate::mods;
use crate::registry::{self, Entry};

/// Largest package download (release assets and marketplace packages).
pub(crate) const MAX_PACKAGE: u64 = 33 * 1024 * 1024;

/// The kernel's [`ModInstaller`] (**Adapter** over `sun_mod::install_package`
/// and the mod registry's GitHub Release assets).
pub struct KernelInstaller {
    /// The kernel's store (install rows, audit, mod-scope secrets).
    pub store: Store,
    /// The data dir (`mods/`, `mod-data/`, `sun.yaml`).
    pub data_dir: PathBuf,
    /// The running mod host installs load into.
    pub host: Arc<ModHost>,
    /// Run mods in the OS sandbox (off: dev and tests).
    pub sandbox: bool,
    /// Keys a signed release asset must match (`SUN_TRUSTED_KEYS`).
    pub trusted_keys: Vec<[u8; 32]>,
    /// The mods `GET /mods` lists and installs ([`registry::entries`]).
    pub registry: Vec<Entry>,
    /// Gives a hot-loaded mod what boot gives a loaded one: its triggers and,
    /// with run hooks (`mods::hooks_of`), a run-hook feed. `None` in tests.
    pub engine: Option<sun_engine::Engine>,
    /// What the mod host runs: install row [`stamp`] per mod name.
    pub loaded: tokio::sync::Mutex<HashMap<String, String>>,
}

/// Changes when a reinstall changes what runs (version, files, grants).
fn stamp(install: &ModInstall) -> String {
    format!(
        "{} {:?} {:?}",
        install.version, install.package_digest, install.granted_permissions
    )
}

fn unavailable(e: impl std::fmt::Display) -> Error {
    Error::Unavailable(e.to_string())
}

/// `granted` must be exactly what `listed` shows: explicit consent.
fn check_consent(listed: &AvailableMod, granted: &[String]) -> Result<()> {
    let mut want = listed.permissions.clone();
    let mut got = granted.to_vec();
    want.sort();
    got.sort();
    if want == got {
        return Ok(());
    }
    let asks = match want.is_empty() {
        true => "nothing".to_owned(),
        false => want.join(", "),
    };
    Err(Error::InvalidInput(format!(
        "The permissions approved differ from what {} {} asks for ({asks}). Reload and review them again.",
        listed.name, listed.version
    )))
}

pub(crate) fn http() -> ureq::Agent {
    ureq::Agent::config_builder()
        .http_status_as_error(false)
        .timeout_global(Some(Duration::from_secs(60)))
        .build()
        .into()
}

/// The install path for a marketplace package (free or paid): checks
/// `archive` against its listing (`package_digest`, name, version,
/// `owner_key` signature) and installs it into `mods_dir` with `granted`.
pub(crate) fn install_archive(
    archive: &[u8],
    mods_dir: &Path,
    name: &str,
    version: &str,
    package_digest: &str,
    owner_key: &str,
    granted: &[String],
) -> Result<ModInstall> {
    let owner_key = mods::parse_key(owner_key)
        .map_err(|e| Error::InvalidInput(format!("registry owner key: {e}")))?;
    std::fs::create_dir_all(mods_dir).map_err(unavailable)?;
    let staging = mods_dir.join(format!(".download-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&staging);
    let result = (|| {
        sun_mod::unpack_package(archive, &staging)?;
        if mods::hex(&sun_mod::package_digest(&staging)?) != package_digest {
            return Err(Error::InvalidInput(format!(
                "{name} {version}: the download does not match the registry index"
            )));
        }
        let manifest = sun_mod::load_manifest(&staging)?;
        if (manifest.name.as_str(), manifest.version.as_str()) != (name, version) {
            return Err(Error::InvalidInput(format!(
                "{name} {version}: the package is {} {}",
                manifest.name, manifest.version
            )));
        }
        sun_mod::install_package(&staging, mods_dir, Some(&[owner_key]), granted)
    })();
    let _ = std::fs::remove_dir_all(&staging);
    result
}

/// Installs the release asset `asset` (a cargo-dist `.tar.gz` holding one
/// package folder) of registry `entry` into `mods_dir` with `granted`,
/// refusing any other mod or version. A signed package must match `keys`.
fn install_asset(
    asset: &[u8],
    mods_dir: &Path,
    entry: &Entry,
    keys: &[[u8; 32]],
    granted: &[String],
) -> Result<ModInstall> {
    let (name, version) = (&entry.name, &entry.version);
    std::fs::create_dir_all(mods_dir).map_err(unavailable)?;
    let staging = mods_dir.join(format!(".download-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&staging);
    let result = (|| {
        std::fs::create_dir_all(&staging).map_err(unavailable)?;
        let file = staging.join("asset.tar.gz");
        std::fs::write(&file, asset).map_err(unavailable)?;
        let invalid = |e: anyhow::Error| Error::InvalidInput(format!("{name} {version}: {e:#}"));
        let out = staging.join("pkg");
        mods::unpack_into(&file, &out).map_err(invalid)?;
        let root = mods::package_root(&out).map_err(invalid)?;
        let manifest = sun_mod::load_manifest(&root)?;
        if manifest.name != *name
            || manifest.version != *version
            || mods::core_kind(manifest.kind) != entry.kind
        {
            return Err(Error::InvalidInput(format!(
                "{name}: the release v{version} contains {} {}",
                manifest.name, manifest.version
            )));
        }
        let signed = root.join(sun_mod::SIGNATURE_FILE).exists();
        sun_mod::install_package(&root, mods_dir, signed.then_some(keys), granted)
    })();
    let _ = std::fs::remove_dir_all(&staging);
    result
}

impl KernelInstaller {
    /// Records the installs boot already loaded (`mods::load_installed`).
    pub async fn seed(&self) -> Result<()> {
        let rows = self.store.read().await?.mod_installs().await?;
        let mut loaded = self.loaded.lock().await;
        *loaded = rows.iter().map(|r| (r.name.clone(), stamp(r))).collect();
        Ok(())
    }

    /// Brings the mod host in line with the install rows: loads new and
    /// changed installs, stops removed ones. A mod that fails to load is
    /// logged and retried only once its row changes.
    pub async fn sync(&self) -> Result<()> {
        let rows = self.store.read().await?.mod_installs().await?;
        let mut loaded = self.loaded.lock().await;
        let gone: Vec<String> = loaded
            .keys()
            .filter(|name| !rows.iter().any(|r| &r.name == *name))
            .cloned()
            .collect();
        for name in gone {
            loaded.remove(&name);
            let _ = self.host.remove(&name).await;
            tracing::info!("mod {name} removed");
        }
        for row in rows {
            let stamp = stamp(&row);
            if loaded.get(&row.name) == Some(&stamp) {
                continue;
            }
            loaded.insert(row.name.clone(), stamp);
            match self.load(&row).await {
                Ok(()) => tracing::info!("mod {} {} loaded", row.name, row.version),
                Err(e) => tracing::warn!("mod {} not loaded: {e}", row.name),
            }
        }
        Ok(())
    }

    /// Polls [`sync`](Self::sync) every `every`, for CLI installs.
    // ponytail: polling; a notify from the CLI to the kernel would be instant.
    pub async fn follow(self: Arc<Self>, every: Duration) {
        loop {
            tokio::time::sleep(every).await;
            if let Err(e) = self.sync().await {
                tracing::warn!("mod sync: {e}");
            }
        }
    }

    /// Loads one install into the running host with what boot gives every
    /// loaded mod (serve.rs): triggers and run hooks.
    async fn load(&self, record: &ModInstall) -> Result<()> {
        let config = Config::load(&self.data_dir).map_err(|e| unavailable(format!("{e:#}")))?;
        let manifest = mods::load_one(
            &self.store,
            &self.data_dir,
            &config,
            &self.host,
            record,
            self.sandbox,
        )
        .await
        .map_err(|e| {
            unavailable(format!(
                "{} is installed but did not load ({e:#}); restart sun to retry",
                record.name
            ))
        })?;
        if let Some(engine) = &self.engine {
            engine
                .subscribe_triggers(std::slice::from_ref(&manifest.name))
                .await?;
            if let Some(hooks) = mods::hooks_of(&manifest) {
                tokio::spawn(engine.clone().feed_observer(manifest.name.clone(), hooks));
            }
        }
        Ok(())
    }
}

#[async_trait]
impl ModInstaller for KernelInstaller {
    async fn available(&self) -> Result<Vec<AvailableMod>> {
        Ok(self.registry.iter().map(registry::listed).collect())
    }

    async fn install(&self, name: &str, granted: &[String], actor: &str) -> Result<ModInstall> {
        let entry = self
            .registry
            .iter()
            .find(|e| e.name == name)
            .cloned()
            .ok_or_else(|| Error::NotFound(format!("mod {name}")))?;
        check_consent(&registry::listed(&entry), granted)?;
        let mods_dir = mods::mods_dir(&self.data_dir);
        let (keys, granted) = (self.trusted_keys.clone(), granted.to_vec());
        let record = tokio::task::spawn_blocking(move || {
            let asset = registry::download(&entry)?;
            install_asset(&asset, &mods_dir, &entry, &keys, &granted)
        })
        .await
        .map_err(unavailable)??;
        self.installed(record, actor).await
    }

    async fn remove(&self, name: &str, purge: bool, actor: &str) -> Result<Vec<String>> {
        let mut loaded = self.loaded.lock().await;
        let needed_by = mods::remove_from(&self.store, &self.data_dir, name, purge, actor)
            .await
            .map_err(|e| match e.downcast::<Error>() {
                Ok(e) => e,
                Err(e) => unavailable(format!("{e:#}")),
            })?;
        loaded.remove(name);
        let _ = self.host.remove(name).await;
        Ok(needed_by)
    }
}

impl KernelInstaller {
    /// Records a package just installed (audited as `actor`) and loads it.
    async fn installed(&self, record: ModInstall, actor: &str) -> Result<ModInstall> {
        mods::record_install(&self.store, &record, actor)
            .await
            .map_err(|e| unavailable(format!("{e:#}")))?;
        let mut loaded = self.loaded.lock().await;
        loaded.insert(record.name.clone(), stamp(&record));
        self.load(&record).await?;
        Ok(record)
    }
}

/// Where this `sun` runs: someone's own machine, or a server.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InstallKind {
    /// The launchd agent or a hand-run kernel.
    Local,
    /// The systemd service (`install.sh` on Linux): devices stay off there.
    Vps,
}

impl InstallKind {
    /// A systemd install is a VPS; anything else is local.
    pub fn detect() -> Self {
        match crate::ops::Service::detect() {
            crate::ops::Service::Systemd => Self::Vps,
            _ => Self::Local,
        }
    }
}

/// The tools behind the local-only features this machine has.
#[derive(Debug, Clone, Copy, Default)]
pub struct LocalTools {
    /// Xcode with the iOS Simulator (`xcrun --find simctl`).
    pub xcode: bool,
    /// The Android SDK emulator, where the host looks for it.
    pub android_sdk: bool,
    /// Lima (`limactl`), the VM backend.
    pub lima: bool,
}

impl LocalTools {
    /// Probes this machine; quick, nothing is booted.
    pub fn detect() -> Self {
        let runs = |program: &Path, args: &[&str]| {
            std::process::Command::new(program)
                .args(args)
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()
                .is_ok_and(|s| s.success())
        };
        let on_path = |program: &Path| {
            (program.is_absolute() && program.is_file())
                || std::env::var_os("PATH").is_some_and(|path| {
                    std::env::split_paths(&path).any(|dir| dir.join(program).is_file())
                })
        };
        // The tools the in-process host would use (its defaults).
        let host = sun_host::HostConfig::new(PathBuf::new());
        Self {
            xcode: runs(&host.devices.xcrun, &["--find", "simctl"]),
            android_sdk: on_path(&host.devices.emulator),
            lima: on_path(&host.vm_cli),
        }
    }
}

/// A local-only feature offered at install time, with what was found or how
/// to get it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Offer {
    /// `simulators` or `vms`.
    pub name: &'static str,
    /// The tools found; empty: not set up yet.
    pub found: Vec<&'static str>,
    /// What it gives, or how to set it up when nothing was found.
    pub note: &'static str,
}

/// What a fresh install offers: simulators and VMs on a local
/// machine, nothing on a VPS (its kernel box runs no emulators or VMs).
pub fn local_offers(kind: InstallKind, tools: LocalTools) -> Vec<Offer> {
    if kind == InstallKind::Vps {
        return Vec::new();
    }
    let found = |pairs: &[(bool, &'static str)]| -> Vec<&'static str> {
        pairs
            .iter()
            .filter(|(on, _)| *on)
            .map(|(_, n)| *n)
            .collect()
    };
    let simulators = found(&[(tools.xcode, "Xcode"), (tools.android_sdk, "Android SDK")]);
    let vms = found(&[(tools.lima, "Lima")]);
    vec![
        Offer {
            name: "simulators",
            note: match simulators.is_empty() {
                false => "tasks can verify on a simulator or emulator",
                true => {
                    "install Xcode or the Android SDK (with an emulator image) to verify on a device"
                }
            },
            found: simulators,
        },
        Offer {
            name: "vms",
            note: match vms.is_empty() {
                false => "sessions can run in a VM (exec_env: vm)",
                true => "install Lima (brew install lima) to run sessions in a VM",
            },
            found: vms,
        },
    ]
}

#[cfg(test)]
mod offer_tests {
    use super::*;

    #[test]
    fn local_install_offers_sim_and_vm_vps_offers_none() {
        let all = LocalTools {
            xcode: true,
            android_sdk: true,
            lima: true,
        };
        let local = local_offers(InstallKind::Local, all);
        let names: Vec<_> = local.iter().map(|o| (o.name, o.found.clone())).collect();
        assert_eq!(
            names,
            [
                ("simulators", vec!["Xcode", "Android SDK"]),
                ("vms", vec!["Lima"])
            ]
        );
        assert!(local_offers(InstallKind::Vps, all).is_empty());

        let bare = local_offers(InstallKind::Local, LocalTools::default());
        assert!(bare.iter().all(|o| o.found.is_empty()));
        assert!(bare[1].note.contains("brew install lima"), "{bare:?}");
    }
}
