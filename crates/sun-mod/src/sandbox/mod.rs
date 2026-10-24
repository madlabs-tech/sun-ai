//! OS sandbox for third-party mods.
//!
//! A [`Sandbox`] is the resolved policy for one mod: what its manifest
//! declares plus what the user granted. [`ModHost::add_sandboxed`] runs the mod
//! inside it:
//!
//! - **Files:** read-only system paths and the mod's folder; read-write only
//!   its data dir (also `HOME`) and the declared `paths`. Everything else,
//!   including the real `HOME`, is denied.
//! - **Network:** no direct connections. Each sandboxed mod gets its own
//!   **CONNECT proxy** ([`proxy`]) on loopback that only tunnels to the
//!   declared `network` hosts; `HTTPS_PROXY`/`HTTP_PROXY`/`ALL_PROXY` point at
//!   it.
//! - **Processes:** a mod may run only programs in its folder, its declared
//!   entry (even outside the folder), the entry's runtime and the declared
//!   `processes` (a name on `PATH` or an absolute
//!   path). An entry script starting `#!/usr/bin/env <interpreter>` runs as
//!   `<interpreter> <script>`, so the interpreter is its runtime. MCP command
//!   servers (npx, uvx launchers) may run any program, still inside the same
//!   file and network jail. A declared multicall binary (uutils coreutils,
//!   busybox) is refused: its other names would run too. On Linux the list
//!   binds what the mod `exec`s, not what its code can do: the dynamic
//!   loader stays executable and runs any readable binary (`ld.so <file>`),
//!   still inside the same jail.
//! - **Secrets:** only granted values of declared secret names, as env vars.
//!   The kernel's own environment never reaches the mod.
//!
//! Enforcement (**Strategy** per OS, chosen at compile time):
//! - macOS: a generated `sandbox-exec` profile ([`macos`]).
//! - Linux: the `sun-mod-sandbox` launcher ([`linux`]): user + network
//!   namespace whose only exit is the proxy, Landlock for paths, seccomp for
//!   syscalls.
//! - Elsewhere: sandboxed mods refuse to start.
//!
//! [`ModHost::add_sandboxed`]: crate::ModHost::add_sandboxed

#[cfg(target_os = "linux")]
pub mod linux;
#[cfg(target_os = "macos")]
mod macos;
pub(crate) mod proxy;

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use sun_core::{Error, Result};
use sun_mod_sdk::Manifest;

/// Env vars that point HTTP clients at the mod's proxy.
const PROXY_VARS: [&str; 6] = [
    "HTTPS_PROXY",
    "https_proxy",
    "HTTP_PROXY",
    "http_proxy",
    "ALL_PROXY",
    "all_proxy",
];

/// What one sandboxed mod may touch. Build it with [`Sandbox::for_mod`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sandbox {
    /// Declared hosts the proxy tunnels to: `host` (port 443) or `host:port`.
    pub network: Vec<String>,
    /// Extra read-write paths (declared `paths`, and `/dev/<name>` for `devices`).
    pub paths: Vec<PathBuf>,
    /// The mod's private read-write dir; also its `HOME`.
    pub data_dir: PathBuf,
    /// Granted secrets, passed as env vars.
    pub secrets: Vec<(String, String)>,
    /// Programs it may run besides its own folder and entry runtime (the
    /// declared `processes`, resolved); `None`: any program.
    pub processes: Option<Vec<PathBuf>>,
    /// Linux only: the `sun-mod-sandbox` launcher binary. Defaults to the one
    /// next to the current executable.
    pub launcher: PathBuf,
}

impl Sandbox {
    /// Resolves the policy for `manifest`. `secrets` are the values the user
    /// granted; each must be a secret the manifest declares. Creates `data_dir`.
    pub fn for_mod(
        manifest: &Manifest,
        data_dir: &Path,
        secrets: Vec<(String, String)>,
    ) -> Result<Self> {
        let perms = &manifest.permissions;
        for host in &perms.network {
            check_host(host)?;
        }
        for (name, _) in &secrets {
            if !perms.secrets.contains(name) {
                return Err(Error::InvalidInput(format!(
                    "mod {}: secret {name} is not declared in mod.yaml",
                    manifest.name
                )));
            }
        }
        let mut paths = Vec::new();
        for path in &perms.paths {
            let path = PathBuf::from(path);
            if !path.is_absolute() {
                return Err(Error::InvalidInput(format!(
                    "mod {}: permission path {} must be absolute",
                    manifest.name,
                    path.display()
                )));
            }
            paths.push(path);
        }
        for device in &perms.devices {
            if device.is_empty() || device.contains(['/', '.']) {
                return Err(Error::InvalidInput(format!("bad device name {device:?}")));
            }
            paths.push(Path::new("/dev").join(device));
        }
        let mut processes = Vec::new();
        for process in &perms.processes {
            let Some(real) = resolve(process) else {
                continue;
            };
            let others = multicall_names(process, &real);
            if !others.is_empty() {
                let more = others.len().saturating_sub(3);
                let mut also = others.into_iter().take(3).collect::<Vec<_>>().join(", ");
                if more > 0 {
                    also.push_str(&format!(" and {more} more"));
                }
                return Err(Error::InvalidInput(format!(
                    "mod {}: process {process} is {}, a multicall binary that also runs as \
                     {also}; the sandbox can't grant one of its names without the others",
                    manifest.name,
                    real.display(),
                )));
            }
            processes.push(real);
        }
        std::fs::create_dir_all(data_dir)
            .map_err(|e| Error::Unavailable(format!("mod data dir {}: {e}", data_dir.display())))?;
        let launcher = std::env::current_exe()
            .ok()
            .and_then(|exe| Some(exe.parent()?.join("sun-mod-sandbox")))
            .unwrap_or_else(|| PathBuf::from("sun-mod-sandbox"));
        Ok(Self {
            network: perms.network.clone(),
            paths,
            data_dir: data_dir.to_path_buf(),
            secrets,
            processes: Some(processes),
            launcher,
        })
    }

    /// The command running `program args` inside the sandbox, with a clean
    /// environment: `HOME`, the proxy vars and the granted secrets only.
    pub(crate) fn command(
        &self,
        program: PathBuf,
        args: &[String],
        mod_dir: &Path,
        proxy_port: u16,
    ) -> Result<tokio::process::Command> {
        // The declared entry as a path (not a name on PATH) may live outside
        // the mod's folder (an absolute `entry.command`): it alone, not its
        // folder, may be read and run.
        let entry = (program.components().count() > 1).then(|| program.clone());
        let (program, args) = match env_shebang(&program) {
            Some(mut argv) => {
                let interpreter = PathBuf::from(argv.remove(0));
                argv.push(program.to_string_lossy().into_owned());
                argv.extend_from_slice(args);
                (interpreter, argv)
            }
            None => (program, args.to_vec()),
        };
        let mut read = vec![mod_dir.to_path_buf()];
        read.extend(runtime_dir(&program));
        read.extend(entry);
        let (program, args) = self.os_wrap(program, &args, &read, proxy_port)?;
        let proxy_url = format!("http://127.0.0.1:{proxy_port}");
        let mut cmd = tokio::process::Command::new(program);
        cmd.args(args)
            .env_clear()
            .env("HOME", &self.data_dir)
            .envs(PROXY_VARS.map(|var| (var, &proxy_url)))
            .envs(self.secrets.iter().map(|(k, v)| (k, v)));
        Ok(cmd)
    }

    #[cfg(target_os = "macos")]
    fn os_wrap(
        &self,
        program: PathBuf,
        args: &[String],
        read: &[PathBuf],
        proxy_port: u16,
    ) -> Result<(PathBuf, Vec<OsString>)> {
        Ok(macos::wrap(self, program, args, read, proxy_port))
    }

    #[cfg(target_os = "linux")]
    fn os_wrap(
        &self,
        program: PathBuf,
        args: &[String],
        read: &[PathBuf],
        proxy_port: u16,
    ) -> Result<(PathBuf, Vec<OsString>)> {
        linux::wrap(self, program, args, read, proxy_port)
    }

    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    fn os_wrap(
        &self,
        _program: PathBuf,
        _args: &[String],
        _read: &[PathBuf],
        _proxy_port: u16,
    ) -> Result<(PathBuf, Vec<OsString>)> {
        Err(Error::Unavailable(
            "no mod sandbox on this OS; refusing to start a sandboxed mod".into(),
        ))
    }
}

/// For a script whose first line is `#!/usr/bin/env [-S] <name> [args]`:
/// `[<name>, args..]`, to run directly. `/usr/bin/env` itself is not an
/// allowed process, and it would look `<name>` up at exec time anyway.
fn env_shebang(program: &Path) -> Option<Vec<String>> {
    use std::io::Read;
    if program.components().count() == 1 {
        return None; // a bare name on PATH, not a script in the mod's folder
    }
    let mut head = [0u8; 256];
    let n = std::fs::File::open(program).ok()?.read(&mut head).ok()?;
    let line = std::str::from_utf8(&head[..n]).ok()?.lines().next()?;
    let mut words = line.strip_prefix("#!")?.split_whitespace();
    if !words.next()?.ends_with("/env") {
        return None;
    }
    let mut argv: Vec<String> = words.map(String::from).collect();
    if argv.first().is_some_and(|w| w == "-S") {
        argv.remove(0);
    }
    let name = argv.first()?;
    (!name.contains('/') && !name.starts_with('-')).then_some(argv)
}

/// A declared process as the real file the OS checks: an absolute path, or
/// a name found on `PATH`. A program that isn't there can't run anyway.
fn resolve(process: &str) -> Option<PathBuf> {
    let path = Path::new(process);
    let found = match path.is_absolute() {
        true => path.to_path_buf(),
        false if path.components().count() == 1 => {
            std::env::split_paths(&std::env::var_os("PATH")?)
                .map(|dir| dir.join(path))
                .find(|candidate| candidate.is_file())?
        }
        false => return None,
    };
    std::fs::canonicalize(found).ok()
}

/// Other programs the file `real` (`process` resolved) runs as. The sandbox
/// grants a file (Landlock: its inode), and a multicall binary picks the
/// program from the name it was started by, so granting one name grants
/// all: hard links beside it (uutils coreutils on Ubuntu >= 26.04), or a
/// symlink to busybox/toybox. Versioned names (`perl`, `perl5.38.2`) are the
/// same program. Hard links in other directories are not found.
fn multicall_names(process: &str, real: &Path) -> Vec<String> {
    use std::os::unix::fs::MetadataExt;
    let Some(name) = Path::new(process).file_name().map(|n| n.to_string_lossy()) else {
        return vec![];
    };
    let Ok(meta) = std::fs::metadata(real) else {
        return vec![];
    };
    let mut names: Vec<String> = Vec::new();
    let file_name = real.file_name().map(|n| n.to_string_lossy().into_owned());
    if let Some(file_name) = file_name.filter(|n| matches!(n.as_str(), "busybox" | "toybox")) {
        names.push(file_name);
    }
    if meta.nlink() > 1 {
        let entries = real.parent().and_then(|dir| std::fs::read_dir(dir).ok());
        names.extend(
            entries
                .into_iter()
                .flatten()
                .flatten()
                .filter(|e| {
                    // DirEntry::metadata doesn't follow symlinks: links only.
                    e.metadata()
                        .is_ok_and(|m| m.ino() == meta.ino() && m.dev() == meta.dev())
                })
                .map(|e| e.file_name().to_string_lossy().into_owned()),
        );
    }
    let same = |other: &str| other.starts_with(&*name) || name.starts_with(other);
    let mut others: Vec<String> = names.into_iter().filter(|n| !same(n)).collect();
    others.sort();
    others.dedup();
    others
}

/// For an entry command found on `PATH` (`node`, `python3`): the install
/// prefix of the runtime, so it can load its own files even when it lives
/// under the (otherwise denied) real home, e.g. `~/.nvm/versions/node/v24`.
/// Just the executable when it isn't in a `<prefix>/bin` layout, or the
/// prefix would be the home dir or `/`.
fn runtime_dir(program: &Path) -> Option<PathBuf> {
    if program.components().count() != 1 {
        return None; // a path inside the mod's folder
    }
    let found = std::env::split_paths(&std::env::var_os("PATH")?)
        .map(|dir| dir.join(program))
        .find(|candidate| candidate.is_file())?;
    let exe = std::fs::canonicalize(found).ok()?;
    let bin = exe.parent()?;
    let prefix = bin.parent()?;
    let home = std::env::home_dir();
    let too_broad = prefix.parent().is_none() || home.is_some_and(|h| h.starts_with(prefix));
    match bin.ends_with("bin") && !too_broad {
        true => Some(prefix.to_path_buf()),
        false => Some(exe),
    }
}

/// Accepts `host` or `host:port`: no scheme, path, wildcard or userinfo.
pub(crate) fn check_host(entry: &str) -> Result<()> {
    let ok = proxy::split_host_port(entry, 443).is_some_and(|(host, _)| {
        !host.is_empty()
            && host
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | ':'))
    });
    match ok {
        true => Ok(()),
        false => Err(Error::InvalidInput(format!(
            "bad network permission {entry:?}: want host or host:port"
        ))),
    }
}

/// Read-only system paths every sandboxed mod needs to load and run.
pub(crate) fn system_read_paths() -> &'static [&'static str] {
    #[cfg(target_os = "macos")]
    return &[
        "/usr",
        "/bin",
        "/sbin",
        "/System",
        "/Library/Apple",
        "/Library/Preferences",
        "/private/etc",
        "/private/var/db/timezone",
        "/opt/homebrew",
    ];
    #[cfg(not(target_os = "macos"))]
    return &[
        "/usr",
        "/bin",
        "/sbin",
        "/lib",
        "/lib64",
        "/lib32",
        "/etc",
        "/opt",
        "/nix/store",
    ];
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse_manifest;

    fn manifest(perms: &str) -> Manifest {
        let yaml = format!(
            "name: m\nkind: plugin\nversion: 0.1.0\napi: '>=1.0 <2.0'\nentry: {{ command: x }}\npermissions: {perms}\n"
        );
        parse_manifest(&yaml).unwrap_or_else(|e| panic!("{e}"))
    }

    #[test]
    fn env_shebang_names_the_interpreter() {
        let dir = std::env::temp_dir().join(format!("sun-shebang-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap_or_else(|e| panic!("{e}"));
        let case = |text: &str| {
            let script = dir.join("mod.ts");
            std::fs::write(&script, text).unwrap_or_else(|e| panic!("{e}"));
            env_shebang(&script)
        };
        assert_eq!(case("#!/usr/bin/env node\n"), Some(vec!["node".into()]));
        assert_eq!(
            case("#!/usr/bin/env -S python3 -u\nprint()\n"),
            Some(vec!["python3".into(), "-u".into()])
        );
        for no in [
            "#!/bin/sh\n",
            "#!/usr/bin/env ./x\n",
            "#!/usr/bin/env\n",
            "x",
        ] {
            assert_eq!(case(no), None, "{no}");
        }
        assert_eq!(env_shebang(Path::new("node")), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The paths the generated rules let the mod execute (beneath).
    #[cfg(target_os = "macos")]
    fn exec_rules(cmd: &tokio::process::Command) -> Vec<PathBuf> {
        let args: Vec<String> = cmd
            .as_std()
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        let line = args[1]
            .lines()
            .find(|l| l.starts_with("(allow process-exec"))
            .unwrap_or_else(|| panic!("no process-exec rule in {}", args[1]));
        line.split('"')
            .skip(1)
            .step_by(2)
            .map(PathBuf::from)
            .collect()
    }

    #[cfg(target_os = "linux")]
    fn exec_rules(cmd: &tokio::process::Command) -> Vec<PathBuf> {
        let policy = cmd.as_std().get_args().next().unwrap_or_default();
        let policy: serde_json::Value =
            serde_json::from_str(&policy.to_string_lossy()).unwrap_or_else(|e| panic!("{e}"));
        policy["exec"]
            .as_array()
            .unwrap_or_else(|| panic!("no exec list in {policy}"))
            .iter()
            .filter_map(|p| p.as_str().map(PathBuf::from))
            .collect()
    }

    /// Regression: an absolute `entry.command` outside the mod's folder
    /// (the e2e `github` mod) was denied exec by its own sandbox.
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn sandbox_exec_rules_allow_the_entry_binary_but_not_an_undeclared_one() {
        let root = std::env::temp_dir().join(format!("sun-entry-exec-{}", std::process::id()));
        let (mod_dir, bin) = (root.join("mod"), root.join("bin"));
        for dir in [&mod_dir, &bin] {
            std::fs::create_dir_all(dir).unwrap_or_else(|e| panic!("{e}"));
        }
        let (entry, undeclared) = (bin.join("entry"), bin.join("other"));
        for file in [&entry, &undeclared] {
            std::fs::write(file, "\x7fELF").unwrap_or_else(|e| panic!("{e}"));
        }
        let sandbox = Sandbox {
            network: vec![],
            paths: vec![],
            data_dir: root.join("data"),
            secrets: vec![],
            processes: Some(vec![]),
            launcher: "sun-mod-sandbox".into(),
        };
        let cmd = sandbox
            .command(entry.clone(), &[], &mod_dir, 1)
            .unwrap_or_else(|e| panic!("{e}"));
        let canonical = |p: &Path| std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
        let rules: Vec<PathBuf> = exec_rules(&cmd).iter().map(|r| canonical(r)).collect();
        let allowed = |p: &Path| rules.iter().any(|r| canonical(p).starts_with(r));
        assert!(allowed(&entry), "entry not executable: {rules:?}");
        assert!(allowed(&mod_dir), "mod folder: {rules:?}");
        assert!(!allowed(&undeclared), "undeclared executable: {rules:?}");
        // Not whole lib dirs: Ubuntu >= 26.04 keeps every coreutil there.
        let coreutil = Path::new("/usr/lib/cargo/bin/coreutils/date");
        assert!(!allowed(coreutil), "lib dir executable: {rules:?}");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Ubuntu >= 26.04: `/bin/echo` and `/bin/date` are hard links to one
    /// uutils binary, so granting `echo` would let a mod run `date`.
    #[test]
    fn sandbox_policy_refuses_a_process_that_is_a_multicall_binary() {
        let root = std::env::temp_dir().join(format!("sun-multicall-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap_or_else(|e| panic!("{e}"));
        let file = |name: &str| {
            std::fs::write(root.join(name), "\x7fELF").unwrap_or_else(|e| panic!("{e}"));
        };
        let link = |from: &str, to: &str| {
            std::fs::hard_link(root.join(from), root.join(to)).unwrap_or_else(|e| panic!("{e}"));
        };
        file("echo");
        link("echo", "date");
        file("perl");
        link("perl", "perl5.38.2");
        file("busybox");
        std::os::unix::fs::symlink(root.join("busybox"), root.join("cat"))
            .unwrap_or_else(|e| panic!("{e}"));
        file("dash");
        std::os::unix::fs::symlink(root.join("dash"), root.join("sh"))
            .unwrap_or_else(|e| panic!("{e}"));
        let declare = |process: &str| {
            let process = root.join(process);
            let m = manifest(&format!("{{ processes: [{}] }}", process.display()));
            Sandbox::for_mod(&m, &root.join("data"), vec![])
        };
        for multicall in ["echo", "date", "cat"] {
            let err = declare(multicall).err();
            assert!(
                matches!(&err, Some(Error::InvalidInput(e)) if e.contains("multicall")),
                "{multicall}: {err:?}"
            );
        }
        for single in ["perl", "perl5.38.2", "sh", "busybox"] {
            let ok = declare(single).unwrap_or_else(|e| panic!("{single}: {e}"));
            assert_eq!(ok.processes.map(|p| p.len()), Some(1), "{single}");
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn sandbox_policy_rejects_undeclared_secrets_and_bad_entries() {
        let dir = std::env::temp_dir().join("sun-sandbox-policy-test");
        let m = manifest("{ network: [api.github.com, 'h:8443'], secrets: [TOKEN] }");
        let ok = Sandbox::for_mod(&m, &dir, vec![("TOKEN".into(), "t".into())]);
        assert!(ok.is_ok());
        let undeclared = Sandbox::for_mod(&m, &dir, vec![("OTHER".into(), "t".into())]);
        assert!(matches!(undeclared, Err(Error::InvalidInput(_))));
        for bad in ["'https://x.com'", "'*.x.com'", "'x.com/a'", "'u@x.com'"] {
            let m = manifest(&format!("{{ network: [{bad}] }}"));
            assert!(Sandbox::for_mod(&m, &dir, vec![]).is_err(), "{bad}");
        }
        let m = manifest("{ paths: [relative/dir] }");
        assert!(Sandbox::for_mod(&m, &dir, vec![]).is_err());
    }
}
