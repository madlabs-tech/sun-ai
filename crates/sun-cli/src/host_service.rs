//! The runtime host as a background service: what `sun host join` installs
//! and `sun host start|stop|leave` drive. A launchd agent `dev.sun.host`
//! (macOS) or a systemd unit `sun-host` (Linux: a user unit, or a system
//! unit as root), each running `sun host run` with the data dir it was
//! joined with, restarted when it dies and started at login/boot.

use std::fmt;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};

use crate::ops::{run, uid};

/// `packaging/launchd/dev.sun.host.plist`.
const PLIST: &str = include_str!("../../../packaging/launchd/dev.sun.host.plist");
/// `packaging/systemd/sun-host.service`.
const UNIT: &str = include_str!("../../../packaging/systemd/sun-host.service");
const LAUNCHD_LABEL: &str = "dev.sun.host";
const SYSTEMD_UNIT: &str = "sun-host";

/// Where this machine keeps the host service.
#[derive(Debug, Clone)]
pub enum HostService {
    /// A per-user launch agent.
    Launchd {
        /// `~/Library/LaunchAgents/dev.sun.host.plist`.
        plist: PathBuf,
    },
    /// A systemd unit.
    Systemd {
        /// `~/.config/systemd/user/sun-host.service`, or as root
        /// `/etc/systemd/system/sun-host.service`.
        unit: PathBuf,
        /// A user unit (`systemctl --user`).
        user: bool,
    },
}

impl HostService {
    /// This machine's.
    pub fn here() -> Result<Self> {
        if cfg!(target_os = "macos") {
            let plist = home()?.join(format!("Library/LaunchAgents/{LAUNCHD_LABEL}.plist"));
            return Ok(Self::Launchd { plist });
        }
        if !cfg!(target_os = "linux") {
            bail!("no host service on this OS; pass --no-service and run `sun host run` yourself");
        }
        Ok(match uid()? == "0" {
            true => Self::Systemd {
                unit: PathBuf::from(format!("/etc/systemd/system/{SYSTEMD_UNIT}.service")),
                user: false,
            },
            false => Self::Systemd {
                unit: home()?.join(format!(".config/systemd/user/{SYSTEMD_UNIT}.service")),
                user: true,
            },
        })
    }

    fn file(&self) -> &Path {
        match self {
            Self::Launchd { plist } => plist,
            Self::Systemd { unit, .. } => unit,
        }
    }

    /// Writes the service running `sun_bin host run` on `data_dir` and
    /// (re)starts it. Idempotent: a second call rewrites it and restarts.
    pub fn install(&self, sun_bin: &Path, data_dir: &Path) -> Result<()> {
        let home = home()?;
        // The service starts elsewhere: a relative --data-dir must not move.
        let data_dir = &std::path::absolute(data_dir)?;
        let path = search_path(sun_bin, &home, std::env::var_os("PATH").as_deref());
        let text = match self {
            Self::Launchd { .. } => render(PLIST, sun_bin, data_dir, &home, &path, "", xml),
            Self::Systemd { user, .. } => {
                let wanted_by = if *user {
                    "default.target"
                } else {
                    "multi-user.target"
                };
                render(UNIT, sun_bin, data_dir, &home, &path, wanted_by, systemd)
            }
        };
        let file = self.file();
        if let Some(dir) = file.parent() {
            std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        }
        std::fs::create_dir_all(data_dir)
            .with_context(|| format!("creating {}", data_dir.display()))?;
        std::fs::write(file, text).with_context(|| format!("writing {}", file.display()))?;
        match self {
            Self::Launchd { plist } => {
                self.stop();
                run(Command::new("launchctl")
                    .args(["bootstrap", &format!("gui/{}", uid()?)])
                    .arg(plist))
            }
            Self::Systemd { user, .. } => {
                systemctl(*user, &["daemon-reload"])?;
                systemctl(*user, &["enable", SYSTEMD_UNIT])?;
                systemctl(*user, &["restart", SYSTEMD_UNIT])?;
                if *user {
                    keep_after_logout();
                }
                Ok(())
            }
        }
    }

    /// Stops it until the next login/boot (`sun host stop`). Not running is
    /// fine.
    pub fn stop(&self) {
        // Not loaded / not running is fine: it is stopped either way.
        let _ = match self {
            Self::Launchd { .. } => Command::new("launchctl")
                .args([
                    "bootout",
                    &format!("gui/{}/{LAUNCHD_LABEL}", uid().unwrap_or_default()),
                ])
                .output(),
            Self::Systemd { user, .. } => systemctl_output(*user, &["stop", SYSTEMD_UNIT]),
        };
    }

    /// Stops and deletes it (`sun host leave`). Not installed is fine.
    pub fn remove(&self) -> Result<()> {
        self.stop();
        if let Self::Systemd { user, .. } = self {
            let _ = systemctl_output(*user, &["disable", SYSTEMD_UNIT]);
        }
        let file = self.file();
        match std::fs::remove_file(file) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
                return Err(e).with_context(|| format!("removing {}", file.display()));
            }
            _ => {}
        }
        if let Self::Systemd { user, .. } = self {
            let _ = systemctl_output(*user, &["daemon-reload"]);
        }
        Ok(())
    }

    /// Whether `join` (or `start`) wrote it.
    pub fn installed(&self) -> bool {
        self.file().exists()
    }

    /// The command that shows its recent output.
    pub fn logs(&self, data_dir: &Path) -> String {
        match self {
            Self::Launchd { .. } => {
                format!("tail -n 50 \"{}\"", data_dir.join("host.log").display())
            }
            Self::Systemd { user: true, .. } => {
                format!("journalctl --user -u {SYSTEMD_UNIT} -n 50")
            }
            Self::Systemd { user: false, .. } => format!("journalctl -u {SYSTEMD_UNIT} -n 50"),
        }
    }
}

impl fmt::Display for HostService {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Launchd { .. } => write!(f, "launchd: {LAUNCHD_LABEL}"),
            Self::Systemd { user: true, .. } => write!(f, "systemd --user: {SYSTEMD_UNIT}"),
            Self::Systemd { user: false, .. } => write!(f, "systemd: {SYSTEMD_UNIT}"),
        }
    }
}

fn home() -> Result<PathBuf> {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .context("$HOME is not set")
}

fn systemctl(user: bool, args: &[&str]) -> Result<()> {
    let mut command = Command::new("systemctl");
    if user {
        command.arg("--user");
    }
    run(command.args(args)).context(match user {
        // `su`/`sudo -u` shells and some containers have no user manager.
        true => "systemd user services are unavailable here; log in as this user (ssh or a desktop session) and retry, or pass --no-service and run `sun host run` under your own supervisor",
        false => "systemd could not run the host service",
    })
}

fn systemctl_output(user: bool, args: &[&str]) -> std::io::Result<std::process::Output> {
    let mut command = Command::new("systemctl");
    if user {
        command.arg("--user");
    }
    command.args(args).output()
}

/// A user unit stops at logout and waits for a login after boot unless the
/// user lingers. Enabling it for yourself needs no root on most distros.
fn keep_after_logout() {
    let lingers = Command::new("loginctl")
        .args([
            "show-user",
            &uid().unwrap_or_default(),
            "--property=Linger",
            "--value",
        ])
        .output()
        .is_ok_and(|o| String::from_utf8_lossy(&o.stdout).trim() == "yes");
    if lingers {
        return;
    }
    let user = std::env::var("USER").unwrap_or_else(|_| "$USER".into());
    let enabled = Command::new("loginctl")
        .arg("enable-linger")
        .output()
        .is_ok_and(|o| o.status.success());
    if enabled {
        println!(
            "sun: enabled lingering for {user}, so the host keeps running after you log out and across reboots"
        );
    } else {
        println!(
            "sun: to keep the host running after you log out and across reboots, run: sudo loginctl enable-linger {user}"
        );
    }
}

/// The service's PATH: `sun`'s own dir, then the joining shell's PATH (where
/// `claude`, `codex` and `git` were found), then the usual dirs, like
/// scripts/install.sh gives the kernel. No duplicates.
fn search_path(sun_bin: &Path, home: &Path, shell: Option<&std::ffi::OsStr>) -> String {
    let own = sun_bin.parent().map(Path::to_path_buf);
    let usual = [
        home.join(".local/bin"),
        "/opt/homebrew/bin".into(),
        "/usr/local/bin".into(),
        "/usr/bin".into(),
        "/bin".into(),
        "/usr/sbin".into(),
        "/sbin".into(),
    ];
    let mut dirs: Vec<PathBuf> = Vec::new();
    for dir in own
        .into_iter()
        .chain(shell.into_iter().flat_map(std::env::split_paths))
        .chain(usual)
    {
        if !dir.as_os_str().is_empty() && !dirs.contains(&dir) {
            dirs.push(dir);
        }
    }
    dirs.iter()
        .map(|d| d.display().to_string())
        .collect::<Vec<_>>()
        .join(":")
}

/// Fills a template's `@…@` placeholders, each value escaped for its format.
fn render(
    template: &str,
    sun_bin: &Path,
    data_dir: &Path,
    home: &Path,
    path: &str,
    wanted_by: &str,
    escape: fn(&str) -> String,
) -> String {
    template
        .replace("@SUN_BIN@", &escape(&sun_bin.display().to_string()))
        .replace("@SUN_DATA_DIR@", &escape(&data_dir.display().to_string()))
        .replace("@HOME@", &escape(&home.display().to_string()))
        .replace("@PATH@", &escape(path))
        .replace("@WANTED_BY@", wanted_by)
}

fn xml(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// For a double-quoted systemd value: `\` and `"` escaped, `%` specifiers off.
fn systemd(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('%', "%%")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn templates_fill_every_placeholder_escaped() {
        let bin = Path::new("/Users/a&b/.local/bin/sun");
        let data = Path::new("/Users/a&b/Library/Application Support/sun");
        let home = Path::new("/Users/a&b");
        let plist = render(PLIST, bin, data, home, "/x:/y", "", xml);
        assert!(!plist.contains("@SUN_") && !plist.contains("@PATH@") && !plist.contains("@HOME@"));
        assert!(plist.contains("<string>/Users/a&amp;b/.local/bin/sun</string>"));
        assert!(
            plist.contains(
                "<string>/Users/a&amp;b/Library/Application Support/sun/host.log</string>"
            )
        );
        assert!(plist.contains("<string>dev.sun.host</string>"));

        let bin = Path::new("/home/a\"b/bin/sun");
        let unit = render(
            UNIT,
            bin,
            Path::new("/home/x/.sun"),
            Path::new("/home/x"),
            "/a%b",
            "default.target",
            systemd,
        );
        assert!(!unit.contains('@'), "{unit}");
        assert!(unit.contains("ExecStart=\"/home/a\\\"b/bin/sun\" host run"));
        assert!(unit.contains("Environment=\"PATH=/a%%b\""));
        assert!(unit.contains("WantedBy=default.target"));
    }

    #[test]
    fn path_puts_sun_first_then_the_shell_without_duplicates() {
        let path = search_path(
            Path::new("/opt/sun/sun"),
            Path::new("/home/x"),
            Some(std::ffi::OsStr::new("/home/x/.nvm/bin::/usr/bin:/opt/sun")),
        );
        assert_eq!(
            path,
            "/opt/sun:/home/x/.nvm/bin:/usr/bin:/home/x/.local/bin:/opt/homebrew/bin:/usr/local/bin:/bin:/usr/sbin:/sbin"
        );
    }
}
