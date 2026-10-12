//! Operating an installed Sun: `sun upgrade [--check]`,
//! `sun rollback`, `sun backup` and `sun doctor`.
//!
//! The installed layout is what `scripts/install.sh` writes: `sun` and
//! `sun-mod-sandbox` in one directory, a systemd unit `sun` (Linux, data in
//! `/var/lib/sun`) or a launchd agent `dev.sun.kernel` (macOS, data in
//! `~/Library/Application Support/sun`).
//!
//! An upgrade is: stage the new binaries next to the running one → ask the
//! new binary which installed mods it can't load (`upgrade --check-local`) →
//! wait until no step is mid-flight → stop the service → `VACUUM INTO`
//! backup → swap the binaries (old ones kept as `*.previous`) → the new
//! binary migrates (`sun migrate`) → start the service. A failed migration
//! rolls back on its own. `upgrade.json` in the data dir records what
//! `sun rollback` restores.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use sun_core::StepStatus;
use sun_store::{RunRepo, Store};

use crate::config::Config;

/// The binaries a release ships, installed side by side.
pub const BINARIES: [&str; 2] = ["sun", "sun-mod-sandbox"];
/// The systemd unit (`packaging/systemd/sun.service`).
pub const SYSTEMD_UNIT: &str = "/etc/systemd/system/sun.service";
/// The launchd label (`packaging/launchd/dev.sun.kernel.plist`).
pub const LAUNCHD_LABEL: &str = "dev.sun.kernel";
/// Where `sun upgrade` downloads from; `$SUN_INSTALL_URL` overrides it.
pub const INSTALL_URL: &str = "https://get-sun.madlabs.tech";
const RECORD: &str = "upgrade.json";

/// The data dir: the flag / `$SUN_DATA_DIR`, else `~/.sun` when it exists,
/// else the installed service's dir when it holds a store, else `~/.sun`.
pub fn data_dir(flag: Option<PathBuf>) -> Result<PathBuf> {
    let home_dir = crate::config::data_dir(flag.clone())?;
    if flag.is_some() || home_dir.exists() {
        return Ok(home_dir);
    }
    Ok(service_data_dir()
        .filter(|dir| dir.join("sun.db").exists())
        .unwrap_or(home_dir))
}

fn service_data_dir() -> Option<PathBuf> {
    if cfg!(target_os = "macos") {
        let home = std::env::var_os("HOME")?;
        Some(Path::new(&home).join("Library/Application Support/sun"))
    } else {
        Some(PathBuf::from("/var/lib/sun"))
    }
}

/// The service manager running the kernel, if `install.sh` set one up.
#[derive(Debug, Clone)]
pub enum Service {
    /// `systemctl {stop,start} sun`.
    Systemd,
    /// `launchctl {bootout,bootstrap} gui/<uid>` with this plist.
    Launchd(PathBuf),
    /// Run by hand: the user restarts it.
    None,
}

impl Service {
    /// The installed one on this machine.
    pub fn detect() -> Self {
        if cfg!(target_os = "linux") && Path::new(SYSTEMD_UNIT).exists() {
            return Self::Systemd;
        }
        if let Some(home) = std::env::var_os("HOME") {
            let plist =
                Path::new(&home).join(format!("Library/LaunchAgents/{LAUNCHD_LABEL}.plist"));
            if cfg!(target_os = "macos") && plist.exists() {
                return Self::Launchd(plist);
            }
        }
        Self::None
    }

    fn stop(&self) -> Result<()> {
        match self {
            Self::Systemd => run(Command::new("systemctl").args(["stop", "sun"])),
            // Not loaded is fine: it is stopped either way.
            Self::Launchd(_) => {
                let _ = Command::new("launchctl")
                    .args(["bootout", &format!("gui/{}/{LAUNCHD_LABEL}", uid()?)])
                    .output();
                Ok(())
            }
            Self::None => Ok(()),
        }
    }

    fn start(&self) -> Result<()> {
        match self {
            Self::Systemd => run(Command::new("systemctl").args(["start", "sun"])),
            Self::Launchd(plist) => run(Command::new("launchctl")
                .args(["bootstrap", &format!("gui/{}", uid()?)])
                .arg(plist)),
            Self::None => {
                println!("sun: no service installed; start `sun serve --with-host` yourself");
                Ok(())
            }
        }
    }
}

/// What `sun rollback` restores (`<data_dir>/upgrade.json`).
#[derive(Debug, Serialize, Deserialize)]
pub struct UpgradeRecord {
    /// The version that was running before.
    pub from_version: String,
    /// The version installed.
    pub to_version: String,
    /// The directory holding the binaries (and their `*.previous`).
    pub bin_dir: PathBuf,
    /// The `VACUUM INTO` copy taken with the kernel stopped.
    pub backup: PathBuf,
}

/// Options of `sun upgrade`.
#[derive(Debug)]
pub struct UpgradeOptions {
    /// Only report what would break.
    pub check: bool,
    /// A folder holding the new `sun` (and `sun-mod-sandbox`) instead of a download.
    pub from: Option<PathBuf>,
    /// The release to download; default the latest.
    pub version: Option<String>,
    /// How long to wait for in-flight steps to reach a checkpoint.
    pub wait: Duration,
}

/// `sun upgrade`, for the binaries running from `bin_dir`.
pub async fn upgrade(
    data_dir: &Path,
    bin_dir: &Path,
    service: &Service,
    options: UpgradeOptions,
) -> Result<()> {
    let staging = bin_dir.join(".sun-upgrade");
    let _ = std::fs::remove_dir_all(&staging);
    std::fs::create_dir_all(&staging)
        .with_context(|| format!("creating {} (run as the owner of sun)", staging.display()))?;
    let result = upgrade_staged(data_dir, bin_dir, &staging, service, options).await;
    let _ = std::fs::remove_dir_all(&staging);
    result
}

async fn upgrade_staged(
    data_dir: &Path,
    bin_dir: &Path,
    staging: &Path,
    service: &Service,
    options: UpgradeOptions,
) -> Result<()> {
    match &options.from {
        Some(from) => {
            for name in BINARIES {
                if from.join(name).exists() {
                    std::fs::copy(from.join(name), staging.join(name))
                        .with_context(|| format!("copying {name} from {}", from.display()))?;
                }
            }
        }
        None => download(staging, options.version.as_deref())?,
    }
    let new = staging.join("sun");
    if !new.exists() {
        bail!("no `sun` binary in the new release");
    }
    let from_version = env!("CARGO_PKG_VERSION").to_owned();
    let to_version = version_of(&new)?;
    println!("sun: {from_version} → {to_version}");

    // The new binary knows what it can load.
    let compatible = Command::new(&new)
        .arg("--data-dir")
        .arg(data_dir)
        .args(["upgrade", "--check-local"])
        .env(UPGRADE_FROM, &from_version)
        .status()
        .context("running the new sun")?
        .success();
    println!("sun: release notes: {INSTALL_URL}/releases/{to_version}");
    if options.check {
        return Ok(());
    }
    if !compatible {
        bail!("not upgrading: update or remove the mods listed above first");
    }

    // A stopped kernel makes no progress (its steps resume on boot): only
    // a running one is waited for, and only a service can be stopped for us.
    if kernel_answers(Config::load(data_dir)?.listen()) {
        if let Service::None = service {
            bail!("sun is running outside a service: stop it, then run `sun upgrade` again");
        }
        wait_for_checkpoint(data_dir, options.wait).await?;
    }
    // ponytail: a step may start between the check and the stop; the engine
    // resumes it from the step journal. A drain API would close the gap.
    service.stop()?;
    let backup = backup(data_dir, &format!("pre-upgrade-{from_version}")).await?;
    println!("sun: backup {}", backup.display());
    let record = UpgradeRecord {
        from_version,
        to_version,
        bin_dir: bin_dir.to_path_buf(),
        backup,
    };
    std::fs::write(data_dir.join(RECORD), serde_json::to_vec_pretty(&record)?)?;
    swap_in(staging, bin_dir)?;

    let migrated = Command::new(bin_dir.join("sun"))
        .arg("--data-dir")
        .arg(data_dir)
        .arg("migrate")
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if !migrated {
        rollback(data_dir, service).await?;
        bail!(
            "the new version failed to migrate the store; rolled back to {}",
            record.from_version
        );
    }
    fix_owner(data_dir);
    service.start()?;
    println!(
        "sun: upgraded to {}; `sun rollback` undoes it",
        record.to_version
    );
    Ok(())
}

/// `sun rollback`: the previous binaries and the pre-upgrade store.
pub async fn rollback(data_dir: &Path, service: &Service) -> Result<UpgradeRecord> {
    let path = data_dir.join(RECORD);
    let text = std::fs::read(&path).map_err(|_| {
        anyhow::anyhow!(
            "nothing to roll back: no upgrade recorded in {}",
            data_dir.display()
        )
    })?;
    let record: UpgradeRecord = serde_json::from_slice(&text)?;
    if !record.bin_dir.join("sun.previous").exists() {
        bail!("no previous binary in {}", record.bin_dir.display());
    }
    service.stop()?;
    for name in BINARIES {
        let previous = record.bin_dir.join(format!("{name}.previous"));
        if previous.exists() {
            std::fs::rename(&previous, record.bin_dir.join(name))?;
        }
    }
    // A stale WAL would replay the newer schema over the restored file.
    for suffix in ["-wal", "-shm"] {
        let _ = std::fs::remove_file(data_dir.join(format!("sun.db{suffix}")));
    }
    let restoring = data_dir.join("sun.db.restore");
    std::fs::copy(&record.backup, &restoring)
        .with_context(|| format!("reading the backup {}", record.backup.display()))?;
    std::fs::rename(&restoring, data_dir.join("sun.db"))?;
    std::fs::remove_file(&path)?;
    fix_owner(data_dir);
    service.start()?;
    Ok(record)
}

/// `sun backup`: a consistent copy of the store (`VACUUM INTO`, safe while
/// the kernel runs) at `<data_dir>/backups/<name>-<unix time>.db`.
pub async fn backup(data_dir: &Path, name: &str) -> Result<PathBuf> {
    if !data_dir.join("sun.db").exists() {
        bail!("no store in {} to back up", data_dir.display());
    }
    let dir = data_dir.join("backups");
    std::fs::create_dir_all(&dir)?;
    let secs = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
    let path = dir.join(format!("{name}-{secs}.db"));
    Store::open(data_dir).await?.backup(&path).await?;
    Ok(path)
}

/// Refuses to start on a `postgres://` `SUN_DATABASE_URL`: Sun
/// stores its data in SQLite only. The URL is never printed (it may hold a
/// password).
pub fn refuse_postgres(data_dir: &Path) -> Result<()> {
    // Trimmed: a pasted or quoted `.env` value is still a Postgres URL.
    let url = std::env::var("SUN_DATABASE_URL").unwrap_or_default();
    let url = url.trim().to_ascii_lowercase();
    if url.starts_with("postgres://") || url.starts_with("postgresql://") {
        bail!(
            "Sun stores its data in SQLite only; Postgres support was removed. \
             Unset SUN_DATABASE_URL to use the SQLite store in {}.",
            data_dir.display()
        );
    }
    Ok(())
}

/// Set by the running `sun` when it asks the new one `upgrade --check-local`:
/// the version being upgraded from.
const UPGRADE_FROM: &str = "SUN_UPGRADE_FROM";

/// What breaks on upgrading to each version, newest last: `(version, what
/// breaks and what to do)`. Add an entry with every release that breaks
/// something (config, CLI, API, mod API).
const BREAKING_CHANGES: &[(&str, &str)] = &[];

/// The entries of `changes` newer than `from` (all of them if `from` isn't semver).
fn breaking_since<'a>(
    changes: &'a [(&'a str, &'a str)],
    from: &str,
) -> Vec<&'a (&'a str, &'a str)> {
    let from = semver::Version::parse(from).ok();
    changes
        .iter()
        .filter(|(version, _)| {
            let version = semver::Version::parse(version).ok();
            from.is_none() || version > from
        })
        .collect()
}

/// The hidden `upgrade --check-local`, run on the new binary: its breaking
/// changes since the running version, and the installed mods it can't load
/// (their `api` range excludes its mod API). Returns how many mods.
pub fn check_local(data_dir: &Path) -> usize {
    println!("sun: this version speaks mod API {}", sun_mod::API_VERSION);
    if let Ok(from) = std::env::var(UPGRADE_FROM) {
        let changes = breaking_since(BREAKING_CHANGES, &from);
        if changes.is_empty() {
            println!("sun: no breaking changes since {from}");
        }
        for (version, change) in changes {
            println!("  breaking in {version}: {change}");
        }
    }
    let Ok(entries) = std::fs::read_dir(crate::mods::mods_dir(data_dir)) else {
        return 0;
    };
    let mut broken = 0;
    for entry in entries.flatten() {
        if let Err(e) = sun_mod::load_manifest(&entry.path()) {
            println!("  incompatible: {e}");
            broken += 1;
        }
    }
    broken
}

/// Waits until no step is dispatched, running, verifying or handing off.
async fn wait_for_checkpoint(data_dir: &Path, limit: Duration) -> Result<()> {
    let store = Store::open(data_dir).await?;
    let started = Instant::now();
    let mut said = false;
    loop {
        let mut tx = store.read().await?;
        let mut busy = 0;
        for run in tx.active_runs().await? {
            busy += tx
                .steps_of_run(run.id)
                .await?
                .iter()
                .filter(|s| in_flight(s.status))
                .count();
        }
        drop(tx);
        if busy == 0 {
            return Ok(());
        }
        if started.elapsed() >= limit {
            bail!("{busy} step(s) still in flight after {limit:?}; retry, or pass a longer --wait");
        }
        if !said {
            println!("sun: waiting for {busy} step(s) to reach a checkpoint…");
            said = true;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

/// Mid-step states; every other state is a checkpoint the engine resumes from.
fn in_flight(status: StepStatus) -> bool {
    use StepStatus::*;
    match status {
        Dispatched | Running | Verifying | HandingOff => true,
        Queued | Succeeded | Failed | LimitHit | Stalled | Looping | BudgetExceeded
        | Interrupted | WaitingApproval | Rejected | Cancelled => false,
    }
}

/// Moves the staged binaries into `bin_dir`, keeping the old ones as `*.previous`.
fn swap_in(staging: &Path, bin_dir: &Path) -> Result<()> {
    for name in BINARIES {
        let new = staging.join(name);
        if !new.exists() {
            continue;
        }
        let target = bin_dir.join(name);
        if target.exists() {
            std::fs::copy(&target, bin_dir.join(format!("{name}.previous")))?;
        }
        set_executable(&new)?;
        // Same directory, so the rename is atomic (and fine while `sun` runs).
        std::fs::rename(&new, &target)
            .with_context(|| format!("replacing {}", target.display()))?;
    }
    Ok(())
}

fn set_executable(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))?;
    Ok(())
}

/// Runs the installer in download-only mode: it fetches the archives and
/// verifies their signature (one owner of that logic: `scripts/install.sh`).
fn download(staging: &Path, version: Option<&str>) -> Result<()> {
    let url = std::env::var("SUN_INSTALL_URL").unwrap_or_else(|_| INSTALL_URL.into());
    let mut command = Command::new("sh");
    command
        .arg("-c")
        .arg(r#"curl -fsSL "$0" | sh -s -- --download-only "$1" ${2:+--version "$2"}"#)
        .arg(&url)
        .arg(staging)
        .arg(version.unwrap_or(""));
    run(&mut command).with_context(|| format!("downloading the release from {url}"))
}

/// `sun 1.2.3` → `1.2.3`.
fn version_of(binary: &Path) -> Result<String> {
    let output = Command::new(binary)
        .arg("--version")
        .output()
        .with_context(|| format!("running {}", binary.display()))?;
    let text = String::from_utf8_lossy(&output.stdout);
    match text.trim().strip_prefix("sun ") {
        Some(version) if output.status.success() => Ok(version.to_owned()),
        _ => bail!("{} is not a sun binary", binary.display()),
    }
}

/// Run as root on a service install: hand what we created back to the
/// data dir's owner, or the kernel can't open its own WAL.
fn fix_owner(data_dir: &Path) {
    if cfg!(target_os = "linux") && uid().is_ok_and(|u| u == "0") {
        // ponytail: GNU chown only, which is what Linux installs have.
        let _ = Command::new("chown")
            .arg("-R")
            .arg(format!("--reference={}", data_dir.display()))
            .arg(data_dir)
            .status();
    }
}

fn uid() -> Result<String> {
    let output = Command::new("id").arg("-u").output()?;
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

fn run(command: &mut Command) -> Result<()> {
    let status = command
        .status()
        .with_context(|| format!("running {command:?}"))?;
    if !status.success() {
        bail!("{command:?} failed ({status})");
    }
    Ok(())
}

/// One `sun doctor` line.
struct Check {
    name: &'static str,
    ok: bool,
    detail: String,
}

/// `sun doctor`: data dir, config, agent CLIs, git, disk, port, backup,
/// sandbox and service. Fails when something Sun needs is missing.
pub fn doctor(data_dir: &Path) -> Result<()> {
    let mut checks = Vec::new();
    let mut check = |name, ok, detail: String| checks.push(Check { name, ok, detail });

    let probe = data_dir.join(".doctor");
    let writable = std::fs::create_dir_all(data_dir).is_ok() && std::fs::write(&probe, b"").is_ok();
    let _ = std::fs::remove_file(&probe);
    check(
        "data dir",
        writable,
        format!(
            "{}{}",
            data_dir.display(),
            if writable { "" } else { " is not writable" }
        ),
    );

    let config = Config::load(data_dir);
    let listen = config
        .as_ref()
        .map_or("127.0.0.1:7700".into(), |c| c.listen().to_owned());
    check(
        "sun.yaml",
        config.is_ok(),
        config
            .as_ref()
            .err()
            .map_or("ok".into(), |e| format!("{e:#}")),
    );

    let git = version("git");
    check(
        "git",
        git.is_some(),
        git.unwrap_or_else(|| "not found: install git".into()),
    );
    let mut runtimes = 0;
    for cli in ["claude", "codex", "opencode"] {
        let found = version(cli);
        runtimes += usize::from(found.is_some());
        // One runtime is enough; the others are optional.
        check(
            cli,
            true,
            found.unwrap_or_else(|| "not found (optional)".into()),
        );
    }
    check(
        "runtimes",
        runtimes > 0,
        match runtimes {
            0 => "no agent CLI: install claude, codex or opencode".into(),
            n => format!("{n} agent CLI(s)"),
        },
    );

    let free = free_gb(data_dir);
    check(
        "disk",
        free.is_some_and(|gb| gb >= 1.0),
        free.map_or("unknown".into(), |gb| {
            format!("{gb:.1} GB free (need 1 GB)")
        }),
    );

    let (port_ok, port_detail) = port(&listen);
    check("port", port_ok, port_detail);

    let (backup_ok, backup_detail) = replication(data_dir);
    check("backup", backup_ok, backup_detail);

    if cfg!(target_os = "linux") {
        let launcher = std::env::current_exe()
            .ok()
            .and_then(|exe| Some(exe.parent()?.join("sun-mod-sandbox")));
        let found = launcher.as_ref().is_some_and(|p| p.exists());
        check(
            "sandbox",
            found,
            match found {
                true => "sun-mod-sandbox next to sun".into(),
                false => "sun-mod-sandbox missing next to sun: mods won't start; reinstall".into(),
            },
        );
    }
    let service = match Service::detect() {
        Service::Systemd => "systemd unit sun".into(),
        Service::Launchd(plist) => format!("launchd {}", plist.display()),
        Service::None => "none (run `sun serve --with-host` by hand)".into(),
    };
    check("service", true, service);
    check(
        "token",
        true,
        match data_dir.join("kernel.json").exists() {
            true => "present".into(),
            false => "not yet (created by the first `sun serve`)".into(),
        },
    );

    let mut failed = 0;
    for c in &checks {
        println!(
            "{:<4} {:<9} {}",
            if c.ok { "ok" } else { "FAIL" },
            c.name,
            c.detail
        );
        failed += usize::from(!c.ok);
    }
    if failed > 0 {
        bail!("{failed} check(s) failed");
    }
    Ok(())
}

/// The first line of `<cli> --version`, if it runs.
fn version(cli: &str) -> Option<String> {
    let output = Command::new(cli).arg("--version").output().ok()?;
    let text = String::from_utf8_lossy(&output.stdout);
    output
        .status
        .success()
        .then(|| text.lines().next().unwrap_or("").trim().to_owned())
}

/// Free space of the filesystem holding `dir` (`df -Pk`).
fn free_gb(dir: &Path) -> Option<f64> {
    let output = Command::new("df").arg("-Pk").arg(dir).output().ok()?;
    let text = String::from_utf8_lossy(&output.stdout);
    let kb: f64 = text
        .lines()
        .nth(1)?
        .split_whitespace()
        .nth(3)?
        .parse()
        .ok()?;
    Some(kb / 1024.0 / 1024.0)
}

/// The Litestream config `sun doctor` reads when `LITESTREAM_CONFIG` is unset.
const LITESTREAM_CONFIG: &str = "/etc/litestream.yml";
/// Touched by `scripts/ha.sh` after each file sync pass.
const FILES_SYNCED: &str = "ha-files-synced";
/// A file sync older than this is stale (`ha.sh` syncs every 10 s by default).
const FILES_STALE: Duration = Duration::from_secs(300);
/// How long a local write gets to reach the replica (Litestream syncs every
/// second) before `sun doctor` calls it lag.
const REPLICA_GRACE: Duration = Duration::from_secs(5);

/// Why the replica is behind, or `None` when it is caught up. `litestream
/// status`'s local TXID is Litestream's own position, so a stalled Litestream
/// reports local == replica; SQLite's WAL is the independent witness: a write
/// (`sun.db-wal` modified) after the last upload has not been shipped, once
/// that upload is older than [`REPLICA_GRACE`] (a healthy Litestream uploads
/// within a second of a write, so under steady writes the newest upload is
/// never that old). Unknown times (no WAL file, an unparsable timestamp)
/// trust the TXIDs.
// ponytail: a TRUNCATE checkpoint touches the WAL without new data and reads
// as lag until the next upload; compare WAL frame counts if that shows up.
fn replica_lag(
    local: u64,
    replica: u64,
    last_upload: Option<SystemTime>,
    db_written: Option<SystemTime>,
    now: SystemTime,
) -> Option<String> {
    if replica < local {
        return Some(format!("{} transaction(s) not uploaded", local - replica));
    }
    let last_upload = last_upload?;
    let gap = db_written?.duration_since(last_upload).ok()?;
    if now.duration_since(last_upload).ok()? < REPLICA_GRACE {
        return None;
    }
    Some(format!(
        "database written {:.1}s after the last upload",
        gap.as_secs_f64()
    ))
}

/// High availability (`scripts/ha.sh`): has Litestream's replica
/// caught up with `sun.db`, and how old is `scripts/ha.sh`'s last file sync?
/// Only checked when a Litestream config exists; run it on the primary.
fn replication(data_dir: &Path) -> (bool, String) {
    let config = std::env::var_os("LITESTREAM_CONFIG")
        .map_or_else(|| PathBuf::from(LITESTREAM_CONFIG), PathBuf::from);
    if !config.exists() {
        return (
            true,
            "no Litestream config (optional, see scripts/ha.sh)".into(),
        );
    }
    // A primary always has sun.db; a waiting standby gets one at takeover.
    // ponytail: a former primary waiting as standby keeps its old sun.db and
    // reads as lagging; compare `ha-epoch` with the replica's if that confuses.
    if !data_dir.join("sun.db").exists() {
        return (
            true,
            "standby: no local database yet (the primary holds it)".into(),
        );
    }
    // `litestream status` matches the path as written: no `//` or `/./`.
    let path: PathBuf = data_dir.join("sun.db").components().collect();
    let litestream = |args: &[&str]| -> Option<serde_json::Value> {
        let output = Command::new("litestream")
            .args(args)
            .arg("-config")
            .arg(&config)
            .arg("-json")
            .arg(&path)
            // The config names the DB as `${SUN_DATA_DIR}/sun.db`: the child
            // gets the dir doctor was asked about, whatever the shell has.
            .env("SUN_DATA_DIR", data_dir)
            .output()
            .ok()?;
        let json = serde_json::from_slice(&output.stdout).ok();
        json.filter(|_| output.status.success())
    };
    let txid = |v: &serde_json::Value| v.as_str().and_then(|s| u64::from_str_radix(s, 16).ok());
    // A commit Litestream has not shipped yet looks like lag: keep looking
    // for REPLICA_GRACE before failing.
    let started = Instant::now();
    let db = loop {
        let status = litestream(&["status"]);
        let local = status.as_ref().and_then(|s| txid(&s[0]["local_txid"]));
        let files = litestream(&["ltx", "-level", "all"]);
        let files = files.as_ref().and_then(|f| f.as_array());
        let replica = files.and_then(|f| f.iter().filter_map(|f| txid(&f["max_txid"])).max());
        let newest = files.and_then(|f| f.iter().filter_map(|f| f["timestamp"].as_str()).max());
        let db = match (local, replica, newest) {
            (Some(local), Some(replica), Some(newest)) => {
                let uploaded = newest
                    .parse::<sun_core::Timestamp>()
                    .ok()
                    .map(SystemTime::from);
                let written = std::fs::metadata(data_dir.join("sun.db-wal"))
                    .and_then(|m| m.modified())
                    .ok();
                let lag = replica_lag(local, replica, uploaded, written, SystemTime::now());
                (
                    lag.is_none(),
                    format!(
                        "replica at txid {replica:x} (local {local:x}), last upload {newest}{}",
                        lag.map_or(String::new(), |lag| format!(", {lag}"))
                    ),
                )
            }
            (None, ..) => (
                false,
                format!(
                    "`litestream status` has no TXID for {}: {}",
                    path.display(),
                    status.map_or(
                        "failed (litestream on PATH? SUN_DB_REPLICA set?)".into(),
                        |s| s.to_string()
                    )
                ),
            ),
            _ => (false, "`litestream ltx` found no replica".into()),
        };
        if db.0 || local.is_none() || started.elapsed() > REPLICA_GRACE {
            break db;
        }
        std::thread::sleep(Duration::from_secs(1));
    };
    let age = std::fs::metadata(data_dir.join(FILES_SYNCED))
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.elapsed().ok());
    let files = age.map_or("files never synced (scripts/ha.sh)".into(), |age| {
        format!("files synced {}s ago", age.as_secs())
    });
    (
        db.0 && age.is_some_and(|age| age < FILES_STALE),
        format!("{}; {files}", db.1),
    )
}

/// Free, or taken by a running kernel: fine. Taken by something else: not.
fn port(listen: &str) -> (bool, String) {
    if std::net::TcpListener::bind(listen).is_ok() {
        return (true, format!("{listen} free"));
    }
    match kernel_answers(listen) {
        true => (true, format!("{listen} in use by a running sun")),
        false => (
            false,
            format!("{listen} in use by another program: set `listen` in sun.yaml"),
        ),
    }
}

/// A Sun kernel answers its API on `listen`.
fn kernel_answers(listen: &str) -> bool {
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .http_status_as_error(false)
        .timeout_global(Some(Duration::from_secs(2)))
        .build()
        .into();
    agent
        .get(format!("http://{listen}/api/v1/tasks"))
        .call()
        .is_ok_and(|r| r.status() == 401 || r.status() == 200)
}

/// `sun status --json`: where this install lives and whether its kernel
/// answers. `url` is the running kernel's (`<data_dir>/address`) or the
/// configured `listen`, reached on loopback. `entitlement` is where plans
/// come from (`cloud` or `community`), by this environment, as `sun serve`
/// decides it.
pub fn status(data_dir: &Path) -> Result<serde_json::Value> {
    let config = Config::load(data_dir)?;
    let reported = std::fs::read_to_string(data_dir.join(crate::serve::ADDRESS_FILE))
        .ok()
        .map(|a| a.trim().to_owned())
        .filter(|a| !a.is_empty());
    let dial = |addr: &str| {
        let addr = addr
            .replace("0.0.0.0", "127.0.0.1")
            .replace("[::]", "[::1]");
        format!("http://{addr}")
    };
    let healthy = |url: &str| {
        ureq::get(format!("{url}/healthz"))
            .config()
            .timeout_global(Some(Duration::from_secs(2)))
            .build()
            .call()
            .is_ok()
    };
    // A stale address file (kernel stopped, restarted elsewhere) loses to `listen`.
    let url = reported
        .as_deref()
        .map(dial)
        .filter(|url| healthy(url))
        .unwrap_or_else(|| dial(config.listen()));
    let running = healthy(&url);
    let token_path = data_dir.join("kernel.json");
    let client = match running && token_path.exists() {
        true => {
            let token = crate::config::KernelSecrets::load_or_create(data_dir)?.token;
            Some(crate::client::Client::new(&url, &token))
        }
        false => None,
    };
    // The kernel's own use apart from each agent session's.
    let resources = client.as_ref().and_then(|client| {
        let kernel = client.get("/kernel").ok()?;
        let hosts = client.get("/hosts").ok()?;
        let sessions: Vec<serde_json::Value> = hosts
            .as_array()?
            .iter()
            .flat_map(|host| {
                let stats = host["session_stats"]
                    .as_array()
                    .cloned()
                    .unwrap_or_default();
                stats.into_iter().map(|mut session| {
                    session["host"] = host["name"].clone();
                    session
                })
            })
            .collect();
        Some(serde_json::json!({ "kernel": kernel, "sessions": sessions }))
    });
    let service = match Service::detect() {
        Service::Systemd => "systemd",
        Service::Launchd(_) => "launchd",
        Service::None => "none",
    };
    Ok(serde_json::json!({
        "data_dir": data_dir,
        "listen": config.listen(),
        "url": url,
        "kernel": if running { "running" } else { "stopped" },
        "token_path": token_path,
        "service": service,
        "entitlement": crate::serve::entitlement_source(|name| std::env::var(name).ok()),
        "resources": resources,
    }))
}

/// `sun status` lines for `resources` (`{kernel, sessions}`): the kernel
/// first, then one line per live agent session.
pub fn resource_lines(resources: &serde_json::Value) -> Vec<String> {
    let usage = |v: &serde_json::Value| {
        let mb = v["rss_bytes"].as_u64().map_or("?".into(), |b| {
            format!("{:.1} MB", b as f64 / (1024.0 * 1024.0))
        });
        let cpu = v["cpu_percent"]
            .as_f64()
            .map_or("?".into(), |c| format!("{c:.1}%"));
        let up = v["uptime_s"].as_u64().unwrap_or(0);
        format!("{mb} RAM, {cpu} CPU, up {up} s")
    };
    let mut lines = vec![format!(
        "{:<12} {}",
        "kernel_use",
        usage(&resources["kernel"])
    )];
    for session in resources["sessions"].as_array().into_iter().flatten() {
        let host = session["host"].as_str().unwrap_or("?");
        let id = session["session_id"].as_str().unwrap_or("?");
        lines.push(format!("{:<12} {host} {id}: {}", "session", usage(session)));
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;
    use sun_core::model::SecretScope;
    use sun_store::SecretRepo;

    #[test]
    fn upgrade_check_lists_breaking_changes_since_the_running_version() {
        let changes = [("0.2.0", "a"), ("0.3.0", "b"), ("1.0.0", "c")];
        let since = |from| -> Vec<&str> {
            breaking_since(&changes, from)
                .iter()
                .map(|(_, c)| *c)
                .collect()
        };
        assert_eq!(since("0.2.0"), ["b", "c"]);
        assert_eq!(since("1.0.0"), Vec::<&str>::new());
        assert_eq!(since("dev"), ["a", "b", "c"]);
    }

    /// A fake `sun` that reports `version` and exits `migrate_status` on migrate.
    fn fake_sun(dir: &Path, version: &str, migrate_status: u8) {
        std::fs::create_dir_all(dir).unwrap();
        let script = format!(
            "#!/bin/sh\ncase \"$*\" in\n  *--version*) echo 'sun {version}' ;;\n  *migrate*) exit {migrate_status} ;;\nesac\n"
        );
        std::fs::write(dir.join("sun"), script).unwrap();
        set_executable(&dir.join("sun")).unwrap();
    }

    async fn secrets(data_dir: &Path) -> Vec<String> {
        let store = Store::open(data_dir).await.unwrap();
        let mut tx = store.read().await.unwrap();
        let mut names: Vec<_> = tx
            .list_secrets(Some(&SecretScope::Mod("m".into())))
            .await
            .unwrap()
            .into_iter()
            .map(|s| s.name)
            .collect();
        names.sort();
        names
    }

    async fn set_secret(data_dir: &Path, name: &str) {
        let store = Store::open(data_dir).await.unwrap();
        let mut tx = store.begin().await.unwrap();
        tx.set_secret(&SecretScope::Mod("m".into()), name, "value-123")
            .await
            .unwrap();
        tx.commit().await.unwrap();
    }

    fn options(from: &Path) -> UpgradeOptions {
        UpgradeOptions {
            check: false,
            from: Some(from.to_path_buf()),
            version: None,
            wait: Duration::from_secs(1),
        }
    }

    #[tokio::test]
    async fn upgrade_swaps_binaries_and_rollback_restores_binary_and_store() {
        let tmp = tempfile::tempdir().unwrap();
        let (data, bin, release) = (
            tmp.path().join("data"),
            tmp.path().join("bin"),
            tmp.path().join("rel"),
        );
        fake_sun(&bin, "0.1.0", 0);
        fake_sun(&release, "9.9.9", 0);
        set_secret(&data, "BEFORE").await;
        std::fs::write(data.join("sun.yaml"), "listen: 127.0.0.1:1\n").unwrap();

        upgrade(&data, &bin, &Service::None, options(&release))
            .await
            .unwrap();
        assert_eq!(version_of(&bin.join("sun")).unwrap(), "9.9.9");
        assert!(!bin.join(".sun-upgrade").exists(), "staging cleaned up");
        set_secret(&data, "AFTER").await;
        assert_eq!(secrets(&data).await, ["AFTER", "BEFORE"]);

        let record = rollback(&data, &Service::None).await.unwrap();
        assert_eq!(
            (record.from_version.as_str(), record.to_version.as_str()),
            (env!("CARGO_PKG_VERSION"), "9.9.9")
        );
        assert_eq!(version_of(&bin.join("sun")).unwrap(), "0.1.0");
        assert_eq!(
            secrets(&data).await,
            ["BEFORE"],
            "store is the pre-upgrade backup"
        );
        assert!(
            rollback(&data, &Service::None).await.is_err(),
            "nothing left to roll back"
        );
    }

    #[tokio::test]
    async fn upgrade_rolls_back_on_its_own_when_the_migration_fails() {
        let tmp = tempfile::tempdir().unwrap();
        let (data, bin, release) = (
            tmp.path().join("data"),
            tmp.path().join("bin"),
            tmp.path().join("rel"),
        );
        fake_sun(&bin, "0.1.0", 0);
        fake_sun(&release, "9.9.9", 1);
        set_secret(&data, "BEFORE").await;
        std::fs::write(data.join("sun.yaml"), "listen: 127.0.0.1:1\n").unwrap();

        let error = upgrade(&data, &bin, &Service::None, options(&release))
            .await
            .unwrap_err();
        assert!(error.to_string().contains("rolled back"), "{error}");
        assert_eq!(version_of(&bin.join("sun")).unwrap(), "0.1.0");
        assert_eq!(secrets(&data).await, ["BEFORE"]);
    }

    #[test]
    fn status_shows_kernel_use_apart_from_each_session() {
        let resources = serde_json::json!({
            "kernel": { "pid": 1, "rss_bytes": 42 * 1024 * 1024, "cpu_percent": 0.5, "uptime_s": 60 },
            "sessions": [{ "host": "vps", "session_id": "s1", "rss_bytes": 300 * 1024 * 1024,
                           "cpu_percent": 12.5, "uptime_s": 42 }],
        });
        assert_eq!(
            resource_lines(&resources),
            [
                "kernel_use   42.0 MB RAM, 0.5% CPU, up 60 s",
                "session      vps s1: 300.0 MB RAM, 12.5% CPU, up 42 s",
            ]
        );
    }

    #[test]
    fn doctor_reports_replica_lag_from_txids_and_the_wal() {
        let at = |secs| Some(UNIX_EPOCH + Duration::from_secs(secs));
        let now = |secs| UNIX_EPOCH + Duration::from_secs(secs);
        // Behind by TXID, whatever the clocks say.
        assert_eq!(
            replica_lag(12, 10, at(100), at(50), now(200)).as_deref(),
            Some("2 transaction(s) not uploaded")
        );
        // Caught up: the last write predates the last upload.
        assert_eq!(replica_lag(10, 10, at(100), at(50), now(200)), None);
        // Healthy under writes: a write right after an upload that is still
        // within the grace (Litestream ships it in a second) is not lag.
        assert_eq!(replica_lag(10, 10, at(100), at(101), now(103)), None);
        // Stalled Litestream: TXIDs agree, the WAL moved after the upload,
        // and the upload is past the grace, whether the write was one
        // (right after the upload) or many (still going on).
        assert_eq!(
            replica_lag(10, 10, at(100), at(101), now(110)).as_deref(),
            Some("database written 1.0s after the last upload")
        );
        assert_eq!(
            replica_lag(10, 10, at(100), at(109), now(110)).as_deref(),
            Some("database written 9.0s after the last upload")
        );
        // No WAL or no parsable upload time: only the TXIDs count.
        assert_eq!(replica_lag(10, 10, at(100), None, now(200)), None);
        assert_eq!(replica_lag(10, 10, None, at(103), now(200)), None);
    }

    #[test]
    fn upgrade_waits_only_for_mid_step_states() {
        assert!(in_flight(StepStatus::Running) && in_flight(StepStatus::Verifying));
        assert!(!in_flight(StepStatus::WaitingApproval) && !in_flight(StepStatus::Queued));
    }
}
