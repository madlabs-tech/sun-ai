//! Mods: loading the installed ones into the kernel's [`ModHost`], and the
//! `sun plugin|extension add|update|list|remove` and `sun mod dev` commands.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use sun_core::model::{AuditAction, ModInstall, SecretScope};
use sun_mod::{ModHost, Sandbox, load_manifest, permission_list};
use sun_store::{AuditRepo, ModInstallRepo, SecretRepo, Store};

use crate::config::{Config, ModSettings};

/// `<data_dir>/mods/<name>`: installed packages.
pub fn mods_dir(data_dir: &Path) -> PathBuf {
    data_dir.join("mods")
}

/// The sandbox of mod `name` for this ecosystem: the manifest's permissions
/// plus the operator's extra hosts, and the granted secrets (from the secret
/// store, mod scope) as env vars.
async fn sandbox_for(
    store: &Store,
    data_dir: &Path,
    install: &ModInstall,
    manifest: &mut sun_mod::Manifest,
    settings: Option<&ModSettings>,
) -> Result<Sandbox> {
    if let Some(settings) = settings {
        manifest
            .permissions
            .network
            .extend(settings.network.iter().cloned());
    }
    let scope = SecretScope::Mod(install.name.clone());
    let mut secrets = Vec::new();
    let mut tx = store.read().await?;
    for name in &manifest.permissions.secrets {
        if !install
            .granted_permissions
            .contains(&format!("secret:{name}"))
        {
            continue;
        }
        match tx.secret_value(&scope, name).await {
            Ok(value) => secrets.push((name.clone(), value)),
            Err(sun_store::StoreError::Core(sun_core::Error::NotFound(_))) => {}
            Err(e) => return Err(e.into()),
        }
    }
    drop(tx);
    let names: Vec<String> = secrets.iter().map(|(name, _)| name.clone()).collect();
    sun_engine::audit_secret_read(store, &scope, &names, "kernel", "mod env").await?;
    let mod_data = data_dir.join("mod-data").join(&install.name);
    Ok(Sandbox::for_mod(manifest, &mod_data, secrets)?)
}

/// Adds every installed mod to `host` and returns the manifests of those
/// that loaded. Mods run sandboxed unless `sandbox` is false (dev and tests
/// only). A mod that fails to load is reported and skipped; the kernel still
/// boots.
pub async fn load_installed(
    store: &Store,
    data_dir: &Path,
    config: &Config,
    host: &ModHost,
    sandbox: bool,
) -> Result<Vec<sun_mod::Manifest>> {
    let installs = store.read().await?.mod_installs().await?;
    let mut loaded = Vec::new();
    for install in installs {
        match load_one(store, data_dir, config, host, &install, sandbox).await {
            Ok(manifest) => loaded.push(manifest),
            Err(e) => tracing::warn!("mod {} not loaded: {e:#}", install.name),
        }
    }
    Ok(loaded)
}

/// Adds one installed mod to `host`, replacing a running copy (boot, and
/// installs from the UI without a restart), and returns its manifest.
pub async fn load_one(
    store: &Store,
    data_dir: &Path,
    config: &Config,
    host: &ModHost,
    install: &ModInstall,
    sandbox: bool,
) -> Result<sun_mod::Manifest> {
    let dir = mods_dir(data_dir).join(&install.name);
    let settings = config.mods.get(&install.name);
    let mut manifest = load_manifest(&dir)?;
    let init = settings.and_then(|s| s.config.clone()).unwrap_or_default();
    let jail = match sandbox {
        true => Some(sandbox_for(store, data_dir, install, &mut manifest, settings).await?),
        false => None,
    };
    host.add_configured(manifest.clone(), dir, jail, init)
        .await?;
    Ok(manifest)
}

/// Records an install: its `mod_install` row and `mod_install` audit entry
/// by `actor`, in one transaction.
pub async fn record_install(store: &Store, record: &ModInstall, actor: &str) -> Result<()> {
    let mut tx = store.begin().await?;
    tx.put_mod_install(record).await?;
    let detail = match &record.signed_by {
        Some(key) => format!("{} signed by {key}", record.version),
        None => format!("{} unsigned (private)", record.version),
    };
    tx.append_audit(actor, AuditAction::ModInstall, &record.name, &detail)
        .await?;
    tx.commit().await?;
    Ok(())
}

/// The run-hook feed `manifest` asks for, or `None`: a plugin
/// with an `observe` action gets the whole log, an extension its declared
/// `hooks` and `metrics`.
pub fn hooks_of(manifest: &sun_mod::Manifest) -> Option<sun_engine::Hooks> {
    let p = &manifest.provides;
    match manifest.kind {
        sun_mod::ModKind::Plugin if p.actions.iter().any(|a| a == sun_engine::hook::OBSERVE) => {
            Some(sun_engine::Hooks::default())
        }
        sun_mod::ModKind::Extension if !p.hooks.is_empty() || !p.metrics.is_empty() => {
            Some(sun_engine::Hooks {
                events: Some(p.hooks.clone()),
                metrics: p.metrics.clone(),
            })
        }
        _ => None,
    }
}

/// sun-mod's manifest kind as the stored one.
pub fn core_kind(kind: sun_mod::ModKind) -> sun_core::model::ModKind {
    match kind {
        sun_mod::ModKind::Plugin => sun_core::model::ModKind::Plugin,
        sun_mod::ModKind::Extension => sun_core::model::ModKind::Extension,
    }
}

/// A package folder to install from; a fetched copy (archive, git) is
/// deleted when this drops.
struct Source {
    dir: PathBuf,
    fetched: Option<PathBuf>,
}

impl Drop for Source {
    fn drop(&mut self) {
        if let Some(dir) = &self.fetched {
            let _ = std::fs::remove_dir_all(dir);
        }
    }
}

/// A folder, a package file (`.tar`, `.tar.gz`, `.tgz`, `.zip`, `.sunpkg`),
/// a `git+https://` (or `git+ssh://`, `git+file://`) URL with an optional
/// `#branch-or-tag`, or a name from the mod registry ([`crate::registry`]).
fn resolve_source(source: &str) -> Result<Source> {
    if let Some(url) = source.strip_prefix("git+") {
        return fetch_git(url);
    }
    let path = PathBuf::from(source);
    if path.is_dir() {
        return Ok(Source {
            dir: path,
            fetched: None,
        });
    }
    if path.is_file() {
        return unpack_file(&path);
    }
    if !source.contains('/')
        && let Some(entry) = crate::registry::find(source)
    {
        return fetch_release(entry);
    }
    bail!(
        "{source}: no such mod folder, package file, git+https:// URL or a name from the mod registry"
    )
}

/// A new, empty scratch folder owned by the returned [`Source`].
fn scratch() -> Result<Source> {
    static NEXT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let root = std::env::temp_dir().join(format!("sun-mod-fetch-{}-{n}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root)?;
    Ok(Source {
        dir: root.join("package"),
        fetched: Some(root),
    })
}

fn fetch_git(url: &str) -> Result<Source> {
    let (url, reference) = match url.split_once('#') {
        Some((url, reference)) => (url, Some(reference)),
        None => (url, None),
    };
    if !["https://", "ssh://", "file://"]
        .iter()
        .any(|scheme| url.starts_with(scheme))
    {
        bail!("git+{url}: use git+https://, git+ssh:// or git+file://");
    }
    let mut source = scratch()?;
    let mut git = std::process::Command::new("git");
    git.args([
        "-c",
        "advice.detachedHead=false",
        "clone",
        "--quiet",
        "--depth",
        "1",
    ])
    .env("GIT_TERMINAL_PROMPT", "0");
    if let Some(reference) = reference {
        if reference.is_empty() || reference.starts_with('-') {
            bail!("git+{url}: bad branch or tag {reference:?}");
        }
        git.args(["--branch", reference]);
    }
    let cloned = git
        .arg("--")
        .arg(url)
        .arg(&source.dir)
        .status()
        .context("running git (is it installed?)")?;
    if !cloned.success() {
        bail!("git clone {url} failed");
    }
    std::fs::remove_dir_all(source.dir.join(".git"))?;
    source.dir = package_root(&source.dir)?;
    Ok(source)
}

/// The registry `entry`'s release asset for this platform, unpacked.
fn fetch_release(entry: &crate::registry::Entry) -> Result<Source> {
    let asset = crate::registry::download(entry)?;
    let mut source = scratch()?;
    let file = source.dir.with_file_name("asset.tar.gz");
    std::fs::write(&file, asset)?;
    unpack_into(&file, &source.dir)?;
    source.dir = package_root(&source.dir)?;
    crate::registry::check_release(entry, &load_manifest(&source.dir)?)?;
    Ok(source)
}

fn unpack_file(file: &Path) -> Result<Source> {
    let mut source = scratch()?;
    unpack_into(file, &source.dir)?;
    source.dir = package_root(&source.dir)?;
    Ok(source)
}

/// Unpacks the package file `file` (`.tar`, `.tar.gz`, `.tgz`, `.zip`,
/// `.sunpkg`) into the new folder `out`.
pub(crate) fn unpack_into(file: &Path, out: &Path) -> Result<()> {
    let name = file.to_string_lossy().to_lowercase();
    if name.ends_with(".sunpkg") {
        sun_mod::unpack_package(&std::fs::read(file)?, out)?;
    } else {
        // System tools; both refuse `..` and absolute member paths, and
        // install refuses symlinks.
        let mut tool = if name.ends_with(".zip") {
            let mut unzip = std::process::Command::new("unzip");
            unzip.arg("-q").arg(file).arg("-d").arg(out);
            unzip
        } else if [".tar", ".tar.gz", ".tgz"]
            .iter()
            .any(|e| name.ends_with(e))
        {
            let mut tar = std::process::Command::new("tar");
            tar.arg("-xf").arg(file).arg("-C").arg(out);
            tar
        } else {
            bail!(
                "{}: not a .tar, .tar.gz, .tgz, .zip or .sunpkg package",
                file.display()
            );
        };
        std::fs::create_dir(out)?;
        let status = tool.status().context("running tar/unzip")?;
        if !status.success() {
            bail!("{}: could not unpack it", file.display());
        }
    }
    Ok(())
}

/// The folder holding mod.yaml: `dir` itself, or its only subfolder (how
/// archives of a folder unpack).
pub(crate) fn package_root(dir: &Path) -> Result<PathBuf> {
    if dir.join(sun_mod::MANIFEST_FILE).is_file() {
        return Ok(dir.to_path_buf());
    }
    let entries: Vec<_> = std::fs::read_dir(dir)?.flatten().collect();
    match &entries[..] {
        [only] if only.path().join(sun_mod::MANIFEST_FILE).is_file() => Ok(only.path()),
        _ => bail!("no mod.yaml at the top of the package"),
    }
}

/// A hex ed25519 key: 32 bytes from 64 hex chars (`--trust`,
/// `SUN_TRUSTED_KEYS`, `host.key`).
pub fn parse_key(hex: &str) -> Result<[u8; 32]> {
    let hex = hex.trim();
    if hex.len() != 64 || !hex.is_ascii() {
        bail!("a key is 64 hex chars");
    }
    let mut key = [0u8; 32];
    for (i, byte) in key.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&hex[2 * i..2 * i + 2], 16).context("a key is hex")?;
    }
    Ok(key)
}

/// `sun plugin|extension add|update <source>` ([`resolve_source`]): shows
/// the requested permissions, asks for approval (unless `yes`), verifies the
/// signature when the package is signed (against `trusted_keys`), copies it
/// in and records the install. `update` requires it to be installed already.
/// A running kernel picks the change up on its own ([`crate::installer`]).
pub async fn install(
    data_dir: &Path,
    source: &str,
    kind: sun_mod::ModKind,
    trusted_keys: &[[u8; 32]],
    yes: bool,
    update: bool,
) -> Result<ModInstall> {
    let source = resolve_source(source)?;
    let src = &source.dir;
    let manifest = load_manifest(src)?;
    if manifest.kind != kind {
        bail!(
            "{} is a {:?}, not a {kind:?}: use `sun {} add`",
            manifest.name,
            manifest.kind,
            command_of(manifest.kind)
        );
    }
    let store = Store::open(data_dir).await?;
    let installed = store.read().await?.mod_installs().await?;
    match installed.iter().find(|i| i.name == manifest.name) {
        Some(old) if update => println!("{}: {} → {}", old.name, old.version, manifest.version),
        None if update => bail!(
            "{} is not installed: use `sun {} add`",
            manifest.name,
            command_of(kind)
        ),
        _ => {}
    }
    let requested = permission_list(&manifest.permissions);
    println!("{} {} asks for:", manifest.name, manifest.version);
    if requested.is_empty() {
        println!("  (nothing)");
    }
    for permission in &requested {
        println!("  {permission}");
    }
    if !yes && !confirm("Install with these permissions?")? {
        bail!("not installed");
    }
    let signed = src.join(sun_mod::SIGNATURE_FILE).exists();
    let trusted = signed.then_some(trusted_keys);
    let record = sun_mod::install_package(src, &mods_dir(data_dir), trusted, &requested)?;
    record_install(&store, &record, "token:local").await?;
    Ok(record)
}

/// The installed extensions that declare plugin `name` (`plugin:<name>`).
pub fn dependents(installs: &[ModInstall], name: &str) -> Vec<String> {
    installs
        .iter()
        .filter(|i| i.needs_plugin(name))
        .map(|i| i.name.clone())
        .collect()
}

/// `sun plugin|extension remove <name>`: forgets the install and deletes its
/// package; its data (`mod-data/`, secrets) stays unless `purge`. Returns
/// the extensions that need it ([`dependents`]): they lose it but stay.
pub async fn remove(data_dir: &Path, name: &str, purge: bool) -> Result<Vec<String>> {
    let store = Store::open(data_dir).await?;
    remove_from(&store, data_dir, name, purge, "token:local").await
}

/// [`remove`] on an open store, audited as `actor`.
pub async fn remove_from(
    store: &Store,
    data_dir: &Path,
    name: &str,
    purge: bool,
    actor: &str,
) -> Result<Vec<String>> {
    let mut tx = store.begin().await?;
    let needed_by = dependents(&tx.mod_installs().await?, name);
    if !tx.delete_mod_install(name).await? {
        return Err(sun_core::Error::NotFound(format!("mod {name} is not installed")).into());
    }
    let detail = match purge {
        true => "removed with its data",
        false => "removed",
    };
    tx.append_audit(actor, AuditAction::ModInstall, name, detail)
        .await?;
    if purge {
        let scope = SecretScope::Mod(name.into());
        for secret in tx.list_secrets(Some(&scope)).await? {
            tx.delete_secret(&scope, &secret.name).await?;
        }
    }
    tx.commit().await?;
    let _ = std::fs::remove_dir_all(mods_dir(data_dir).join(name));
    if purge {
        let _ = std::fs::remove_dir_all(data_dir.join("mod-data").join(name));
    }
    Ok(needed_by)
}

/// The installed mods of one kind.
pub async fn list(data_dir: &Path, kind: sun_core::model::ModKind) -> Result<Vec<ModInstall>> {
    let store = Store::open(data_dir).await?;
    let mut installs = store.read().await?.mod_installs().await?;
    installs.retain(|i| i.kind == kind);
    Ok(installs)
}

/// `sun mod dev <dir>`: runs the mod from its folder and reloads it on every
/// change, calling `health` after each reload so a broken build shows at once.
pub async fn dev(dir: &Path) -> Result<()> {
    let host = ModHost::new(sun_mod::HostConfig::default());
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let watch = sun_mod::watch_dev(&host, dir, None, Duration::from_millis(500), |r| {
        let _ = tx.send(r);
    });
    let report = async {
        while let Some(reload) = rx.recv().await {
            match reload {
                Ok(name) => match host.health(&name).await {
                    Ok(health) => println!("reloaded {name}: health {health}"),
                    Err(e) => println!("reloaded {name}: health failed: {e}"),
                },
                Err(e) => println!("not reloaded: {e}"),
            }
        }
    };
    tokio::select! {
        () = watch => {}
        () = report => {}
        _ = tokio::signal::ctrl_c() => {}
    }
    host.shutdown().await;
    Ok(())
}

pub(crate) fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn command_of(kind: sun_mod::ModKind) -> &'static str {
    match kind {
        sun_mod::ModKind::Plugin => "plugin",
        sun_mod::ModKind::Extension => "extension",
    }
}

pub fn confirm(question: &str) -> Result<bool> {
    use std::io::Write;
    print!("{question} [y/N] ");
    std::io::stdout().flush()?;
    let mut answer = String::new();
    std::io::stdin().read_line(&mut answer)?;
    Ok(matches!(answer.trim(), "y" | "Y" | "yes"))
}

#[cfg(test)]
mod tests {
    #[test]
    fn hooks_follow_the_manifest() {
        let parse = |kind: &str, provides: &str| {
            sun_mod::parse_manifest(&format!(
                "name: m\nkind: {kind}\nversion: 1.0.0\napi: \">=1.2 <2.0\"\n\
                 entry: {{ command: ./m }}\nprovides: {{ {provides} }}\n"
            ))
            .unwrap()
        };
        let observer = parse("plugin", "actions: [observe]");
        assert_eq!(super::hooks_of(&observer), Some(Default::default()));
        assert_eq!(super::hooks_of(&parse("plugin", "actions: [ping]")), None);
        let ext = parse("extension", "hooks: [run_finished], metrics: [lines]");
        let hooks = super::hooks_of(&ext).unwrap();
        assert_eq!(hooks.events, Some(vec!["run_finished".to_owned()]));
        assert_eq!(hooks.metrics, ["lines"]);
        assert_eq!(super::hooks_of(&parse("extension", "gates: [g]")), None);
    }

    use std::path::{Path, PathBuf};
    use std::process::Command;

    use sun_mod::ModKind::{Extension, Plugin};

    /// A mod folder `<root>/<name>` with this kind, version and permissions.
    fn package(root: &Path, name: &str, kind: &str, version: &str, perms: &str) -> PathBuf {
        let dir = root.join(name);
        std::fs::create_dir_all(&dir).unwrap();
        let provides = match kind {
            "plugin" => "actions: [ping]",
            _ => "step_types: [ping]",
        };
        let yaml = format!(
            "name: {name}\nkind: {kind}\nversion: {version}\napi: \">=1.0 <2.0\"\n\
             entry: {{ command: ./run }}\nprovides: {{ {provides} }}\npermissions: {perms}\n"
        );
        std::fs::write(dir.join("mod.yaml"), yaml).unwrap();
        std::fs::write(dir.join("run"), "#!/bin/sh\n").unwrap();
        dir
    }

    fn run(cmd: &mut Command) {
        assert!(cmd.status().unwrap().success(), "{cmd:?}");
    }

    #[tokio::test]
    async fn remove_warns_with_dependent_extension() {
        let tmp = tempfile::tempdir().unwrap();
        let (data, src) = (tmp.path().join("data"), tmp.path().join("src"));
        let plugin = package(&src, "notifier", "plugin", "1.0.0", "{}");
        let ext = package(
            &src,
            "relay",
            "extension",
            "1.0.0",
            "{ plugins: [notifier] }",
        );
        for (dir, kind) in [(&plugin, Plugin), (&ext, Extension)] {
            let source = dir.to_str().unwrap();
            super::install(&data, source, kind, &[], true, false)
                .await
                .unwrap();
        }
        assert_eq!(
            super::remove(&data, "notifier", false).await.unwrap(),
            ["relay"]
        );
        assert!(!data.join("mods/notifier").exists());
        // The extension stays; removing what nothing needs warns about nothing.
        assert!(
            super::remove(&data, "relay", true)
                .await
                .unwrap()
                .is_empty()
        );
        let err = super::remove(&data, "relay", false).await.unwrap_err();
        assert!(err.to_string().contains("not installed"), "{err}");
    }

    #[tokio::test]
    async fn add_from_git_url() {
        if Command::new("git").arg("--version").output().is_err() {
            eprintln!("skipped: needs git on PATH");
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let data = tmp.path().join("data");
        let repo = package(tmp.path(), "notifier", "plugin", "1.2.0", "{}");
        let git = |args: &[&str]| {
            run(Command::new("git")
                .args(["-c", "user.name=t", "-c", "user.email=t@t", "-C"])
                .arg(&repo)
                .args(args))
        };
        git(&["init", "--quiet"]);
        git(&["add", "."]);
        git(&["commit", "--quiet", "-m", "v1.2.0"]);
        git(&["tag", "v1.2.0"]);
        let url = format!("git+file://{}#v1.2.0", repo.display());
        let record = super::install(&data, &url, Plugin, &[], true, false)
            .await
            .unwrap();
        assert_eq!(
            (record.name.as_str(), record.version.as_str()),
            ("notifier", "1.2.0")
        );
        let installed = data.join("mods/notifier");
        assert!(installed.join("mod.yaml").exists() && !installed.join(".git").exists());

        // An update from a .tar of the folder replaces it in place.
        package(tmp.path(), "notifier", "plugin", "1.3.0", "{}");
        let tar = tmp.path().join("notifier.tar");
        run(Command::new("tar")
            .arg("-cf")
            .arg(&tar)
            .arg("-C")
            .arg(tmp.path())
            .args(["--exclude", ".git", "notifier"]));
        let source = tar.to_str().unwrap();
        let record = super::install(&data, source, Plugin, &[], true, true)
            .await
            .unwrap();
        assert_eq!(record.version, "1.3.0");
        let wrong_scheme = super::install(&data, "git+ext::sh -c x", Plugin, &[], true, false);
        assert!(wrong_scheme.await.is_err());
    }
}
