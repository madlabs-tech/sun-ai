//! The `vm` environment: one Lima VM per session. Lima runs
//! Virtualization.framework (`vz`) on macOS and QEMU with KVM on Linux.
//!
//! `prepare` runs `limactl start --name sun-<session_id> --tty=false --set
//! <expr> template://<template>`. The expression replaces the template's
//! mounts (so the host home dir is not shared) with the worktree, its git
//! dir, the session `TMPDIR` and the MCP config's dir (writable), and the
//! session's env dir (read-only), each at its host path, and applies the
//! caps. Lima's default user has passwordless `sudo`: the agent has root
//! inside the VM only.
//!
//! Lima reaches the VM over ssh, which forwards no env, so the session env is
//! written to `session.env` (mode 0600) in the env dir and sourced inside;
//! values never appear in an argv. Each agent CLI gets a shim (see [`super`])
//! running `limactl shell --workdir <worktree> <vm> sh -c '. <env> && exec
//! <cli> "$@"'`; `exec` runs a 0600 script with the request's env and
//! command the same way.
//!
//! With a network allowlist ([`VmSpec::network`]) the filter is enforced on
//! the host ([`egress`]): the VM runs on QEMU whose user netdev is
//! `restrict=on`, and its one exit is a `guestfwd` to a host CONNECT proxy
//! that tunnels only to the allowed hosts. `HTTPS_PROXY` & co. point at it
//! and there is no DNS inside. The agent keeps root (root and an
//! allowlist together); flushing the VM's firewall opens nothing. On a
//! marketplace host ([`Vm::drop_root`]) the Lima user's `sudo` is removed as
//! well and the proxy refuses private addresses.
//! `teardown` deletes the VM (reset: the next task starts from a fresh
//! template), or with [`VmSpec::keep`] stops it and leaves it as a snapshot.
//! A kept allowlisted VM is also locked ([`Vm::stop_locked`]) so a hand
//! restart can't boot it without its filter; if the lock fails it is deleted.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use async_trait::async_trait;
use serde_json::{Value, json};
use sun_core::host_protocol::{ExecOutput, ExecRequest};
use sun_core::{Error, ExecEnvSpec, Result, SessionId, SessionSpec, VmSpec};
use tokio::process::Command;

use super::egress::{self, Egress, GUEST_PROXY};
use super::{
    ExecEnv, forwarded, install_shims, mounts, remove_dir, run_cli, run_cli_env, sh_quote,
    write_file,
};
use crate::process;

/// A session's VM (`limactl` CLI).
pub struct Vm {
    cli: PathBuf,
    spec: VmSpec,
    name: String,
    /// Shims, `session.env` and exec scripts; mounted read-only in the VM.
    dir: PathBuf,
    execs: AtomicU64,
    /// Also take `sudo` away once the allowlist is up (marketplace hosts).
    pub(crate) drop_root: bool,
    /// The host-side egress proxy of an allowlisted VM, while it runs.
    egress: std::sync::Mutex<Option<Egress>>,
}

impl Vm {
    /// Validates `spec`; nothing starts until [`ExecEnv::prepare`].
    pub fn new(cli: PathBuf, spec: VmSpec, session_id: SessionId, dir: PathBuf) -> Result<Self> {
        ExecEnvSpec::Vm(spec.clone()).validate()?;
        let hosts = spec.network.as_deref().unwrap_or_default();
        if hosts.len() > MAX_ALLOWED_HOSTS {
            return Err(Error::InvalidInput(format!(
                "at most {MAX_ALLOWED_HOSTS} hosts in a vm network allowlist"
            )));
        }
        if let Some(bad) = hosts.iter().find(|h| !host_name(h)) {
            return Err(Error::InvalidInput(format!(
                "bad host `{bad}` in the vm network allowlist"
            )));
        }
        Ok(Self {
            cli,
            spec,
            name: format!("sun-{}", session_id.to_string().to_lowercase()),
            dir,
            execs: AtomicU64::new(0),
            drop_root: false,
            egress: std::sync::Mutex::new(None),
        })
    }

    /// `limactl start` arguments for a VM with `mounts` (path, writable).
    fn start_args(&self, mounts: &[(PathBuf, bool)]) -> Result<Vec<String>> {
        let mut list = Vec::new();
        for (path, writable) in mounts {
            let location = path.to_str().ok_or_else(|| {
                Error::InvalidInput(format!("cannot mount {}: not UTF-8", path.display()))
            })?;
            list.push(json!({ "location": location, "writable": writable }));
        }
        let mut expr = format!(".mounts = {}", Value::Array(list));
        if let Some(mb) = self.spec.memory_mb {
            expr.push_str(&format!(" | .memory = \"{mb}MiB\""));
        }
        if let Some(cpus) = self.spec.cpus {
            expr.push_str(&format!(" | .cpus = {cpus}"));
        }
        if self.spec.network.is_some() {
            // Egress is enforced on QEMU's netdev; 9p needs no package
            // installs (reverse-sshfs would, and the VM has no internet).
            expr.push_str(" | .vmType = \"qemu\" | .mountType = \"9p\"");
        }
        Ok(vec![
            "start".into(),
            "--name".into(),
            self.name.clone(),
            "--tty=false".into(),
            "--set".into(),
            expr,
            format!("template://{}", self.spec.template),
        ])
    }

    /// The shim that runs `cli` inside the VM with the env from `env_file`.
    fn shim(&self, cli: &str, workdir: &Path, env_file: &Path) -> String {
        format!(
            "#!/bin/sh\nexec {} shell --workdir {} {} sh -c {} {} \"$@\"\n",
            sh_quote(&self.cli.to_string_lossy()),
            sh_quote(&workdir.to_string_lossy()),
            sh_quote(&self.name),
            sh_quote(&format!(". \"$0\" && exec {cli} \"$@\"")),
            sh_quote(&env_file.to_string_lossy()),
        )
    }

    /// `limactl delete -f` of this session's VM (stops it first; a missing
    /// VM is fine).
    async fn delete(&self) -> Result<String> {
        let args = ["delete".into(), "-f".into(), self.name.clone()];
        run_cli(&self.cli, &args).await
    }

    /// Runs `text` as a script with `sh` inside the VM (as the Lima user).
    async fn run_script(&self, file: &str, text: &str) -> Result<String> {
        let script = self.dir.join(file);
        write_file(&script, text, 0o600)?;
        let args = [
            "shell".into(),
            self.name.clone(),
            "sh".into(),
            script.to_string_lossy().into_owned(),
        ];
        let output = run_cli(&self.cli, &args).await;
        let _ = std::fs::remove_file(&script);
        output
    }

    /// Starts the host egress proxy for `hosts` and writes the QEMU wrapper
    /// that makes it the VM's only exit; returns the env `limactl start`
    /// needs. `nc` is the host's netcat (one per guest connection).
    async fn open_egress(&self, hosts: &[String], nc: &Path) -> Result<Vec<(String, String)>> {
        let proxy = egress::start(hosts, self.drop_root)
            .await
            .map_err(|e| Error::Unavailable(format!("cannot start the vm egress proxy: {e}")))?;
        let forward = egress::guestfwd(nc, proxy.port)
            .ok_or_else(|| Error::Unavailable(format!("unusable netcat path {}", nc.display())))?;
        let arch = std::env::consts::ARCH;
        let wrapper = self.dir.join("qemu.sh");
        let qemu = format!("qemu-system-{arch}");
        write_file(&wrapper, &egress::qemu_wrapper(&qemu, &forward), 0o700)?;
        if let Ok(mut slot) = self.egress.lock() {
            *slot = Some(proxy);
        }
        let key = format!("QEMU_SYSTEM_{}", arch.to_ascii_uppercase());
        Ok(vec![(key, sh_quote(&wrapper.to_string_lossy()))])
    }

    /// Stops a kept allowlisted VM and locks it: its egress filter is the
    /// QEMU wrapper and the host proxy, which a hand `limactl start` would
    /// skip, so its `lima.yaml` is renamed to [`LOCKED`] and Lima refuses to
    /// start it until someone renames it back on purpose.
    // ponytail: no `sun` command resumes a kept VM with its filter; add one
    // (proxy + wrapper + unlock) when resuming them is a real workflow.
    async fn stop_locked(&self) -> Result<()> {
        run_cli(&self.cli, &["stop".into(), self.name.clone()]).await?;
        let args = [
            "list".into(),
            "--format".into(),
            "{{.Dir}}".into(),
            self.name.clone(),
        ];
        let dir = PathBuf::from(run_cli(&self.cli, &args).await?.trim());
        if !dir.is_absolute() {
            return Err(Error::Unavailable(format!(
                "limactl list gave no instance dir for {}",
                self.name
            )));
        }
        std::fs::rename(dir.join("lima.yaml"), dir.join(LOCKED)).map_err(super::io(&dir))
    }
}

/// What a kept allowlisted VM's `lima.yaml` is renamed to (see
/// [`Vm::stop_locked`]).
const LOCKED: &str = "lima.yaml.sun-egress-locked";

/// Most hosts in one [`VmSpec::network`] allowlist.
const MAX_ALLOWED_HOSTS: usize = 64;

/// A DNS host name or IPv4 literal: ASCII alphanumerics, `-` and `.`.
fn host_name(host: &str) -> bool {
    (1..=253).contains(&host.len())
        && host.split('.').all(|label| {
            !label.is_empty()
                && !label.starts_with('-')
                && label.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
        })
}

/// Shell script (run in the VM by the Lima user) that removes its `sudo`
/// and fails if it still works (marketplace hosts).
const DROP_ROOT: &str = "set -eu\n\
    sudo sh -c 'rm -f /etc/sudoers.d/*; for g in sudo admin wheel; do \
    gpasswd -d \"$1\" $g >/dev/null 2>&1 || true; done' sh \"$(id -un)\"\n\
    sudo -K\n\
    if sudo -n true 2>/dev/null; then echo 'sudo still works' >&2; exit 1; fi\n";

/// `HTTPS_PROXY` & co. pointing at the guest end of the egress proxy.
fn proxy_env() -> String {
    let url = format!("http://{GUEST_PROXY}");
    let mut text = String::new();
    for name in [
        "HTTPS_PROXY",
        "https_proxy",
        "HTTP_PROXY",
        "http_proxy",
        "ALL_PROXY",
        "all_proxy",
    ] {
        text.push_str(&format!("export {name}={url}\n"));
    }
    text.push_str("export NO_PROXY=localhost,127.0.0.1 no_proxy=localhost,127.0.0.1\n");
    text
}

/// The first `name` file on the host's `PATH`.
fn on_path(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(name))
        .find(|p| p.is_file())
}

#[async_trait]
impl ExecEnv for Vm {
    async fn prepare(&self, spec: &mut SessionSpec) -> Result<()> {
        // Lima mounts dirs only: a file (the MCP config) brings its dir.
        let mut wanted: Vec<(PathBuf, bool)> = mounts(spec)
            .into_iter()
            .filter_map(|p| match p.is_file() {
                true => p.parent().map(|d| (d.to_path_buf(), true)),
                false => Some((p, true)),
            })
            .collect();
        wanted.push((self.dir.clone(), false));
        // Lima rejects overlapping mounts: keep each outermost dir once.
        let mut chosen: Vec<(PathBuf, bool)> = Vec::new();
        for (path, writable) in &wanted {
            let nested = wanted.iter().any(|(o, _)| o != path && path.starts_with(o));
            if !nested && !chosen.iter().any(|(c, _)| c == path) {
                chosen.push((path.clone(), *writable));
            }
        }
        let start = self.start_args(&chosen)?;

        std::fs::create_dir_all(&self.dir).map_err(super::io(&self.dir))?;
        let env_file = self.dir.join("session.env");
        let names = forwarded(spec);
        let env = names.iter().filter_map(|n| Some((n, spec.env.get(n)?)));
        let mut text = exports(env)?;
        let mut cli_env = Vec::new();
        if let Some(hosts) = &self.spec.network {
            let nc = on_path("nc").ok_or_else(|| {
                Error::Unavailable("a vm network allowlist needs `nc` on the host".into())
            })?;
            cli_env = self.open_egress(hosts, &nc).await?;
            text.push_str(&proxy_env());
        }
        write_file(&env_file, &text, 0o600)?;

        // A VM left over from a crashed host would block the name.
        let _ = self.delete().await;
        run_cli_env(&self.cli, &start, &cli_env).await?;
        if self.drop_root {
            self.run_script("drop_root.sh", DROP_ROOT).await?;
        }

        let worktree = spec.worktree.clone();
        install_shims(&self.dir, spec, |cli| self.shim(cli, &worktree, &env_file))
    }

    async fn exec(&self, request: &ExecRequest) -> Result<ExecOutput> {
        let n = self.execs.fetch_add(1, Ordering::Relaxed) + 1;
        let script = self.dir.join(format!("exec-{n}.sh"));
        let text = format!("{}{}\n", exports(request.env.iter())?, request.command);
        write_file(&script, &text, 0o600)?;
        let mut cmd = Command::new(&self.cli);
        cmd.args(["shell", "--workdir"])
            .arg(&request.cwd)
            .args([&self.name, "sh"])
            .arg(&script);
        // ponytail: a timeout kills the limactl client, not the command in
        // the VM; it dies at teardown. Wrap in `timeout` if gates hang.
        let output = process::run(cmd, request).await;
        let _ = std::fs::remove_file(&script);
        output
    }

    async fn teardown(&self) -> Result<()> {
        let released = match (self.spec.keep, self.spec.network.is_some()) {
            (true, false) => run_cli(&self.cli, &["stop".into(), self.name.clone()]).await,
            // Fail closed: a kept VM that can't be locked is not kept.
            (true, true) => match self.stop_locked().await {
                Ok(()) => Ok(String::new()),
                Err(e) => {
                    eprintln!("sun-host: {}: {e}; deleting it instead", self.name);
                    self.delete().await
                }
            },
            (false, _) => self.delete().await,
        };
        if let Ok(mut slot) = self.egress.lock() {
            slot.take();
        }
        remove_dir(&self.dir)?;
        released.map(drop)
    }
}

/// `export NAME='value'` lines for `env`; rejects names that aren't shell
/// identifiers (they would be code inside the VM).
fn exports<'a>(env: impl Iterator<Item = (&'a String, &'a String)>) -> Result<String> {
    let env: BTreeMap<_, _> = env.collect();
    let mut text = String::new();
    for (name, value) in env {
        let ok = name.starts_with(|c: char| c.is_ascii_alphabetic() || c == '_')
            && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
        if !ok {
            return Err(Error::InvalidInput(format!("bad env var name `{name}`")));
        }
        text.push_str(&format!("export {name}={}\n", sh_quote(value)));
    }
    Ok(text)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use std::os::unix::fs::PermissionsExt;

    use sun_core::{EXEC_SHIMS_ENV, ExecEnvSpec};

    use super::super::testing::{FAKE_CLAUDE, fixture, gate, run_claude, script, succeeds};
    use super::*;

    fn spec(template: &str) -> VmSpec {
        VmSpec {
            template: template.into(),
            memory_mb: Some(2048),
            cpus: Some(2),
            network: None,
            keep: false,
        }
    }

    fn open(cli: PathBuf, dir: &Path, spec: &SessionSpec) -> Vm {
        let ExecEnvSpec::Vm(vm) = &spec.exec_env else {
            panic!()
        };
        Vm::new(cli, vm.clone(), spec.session_id, dir.join("env")).unwrap()
    }

    /// The program an adapter with an absolute binary path starts
    /// (`sun_runtime::process::program`).
    fn shimmed(spec: &SessionSpec) -> PathBuf {
        Path::new(&spec.env[EXEC_SHIMS_ENV]).join("claude")
    }

    #[test]
    fn vm_spec_rejects_bad_input() {
        let new = |s: VmSpec| Vm::new("limactl".into(), s, SessionId::generate(), "/x".into());
        for good in ["default", "ubuntu-lts", "docker", "experimental/ubuntu.v2"] {
            assert!(new(spec(good)).is_ok(), "{good}");
        }
        for bad in ["", "-x", "../etc", "/abs", "a b", "a;b", "a/../b"] {
            assert!(new(spec(bad)).is_err(), "{bad:?}");
        }
        assert!(
            new(VmSpec {
                cpus: Some(0),
                ..spec("default")
            })
            .is_err()
        );
        let wire: ExecEnvSpec =
            serde_json::from_str(r#"{"kind":"vm","template":"default","cpus":2}"#).unwrap();
        let want = VmSpec {
            memory_mb: None,
            ..spec("default")
        };
        assert_eq!(wire, ExecEnvSpec::Vm(want));

        let bad = BTreeMap::from([("A;rm -rf /".to_string(), "x".to_string())]);
        assert!(exports(bad.iter()).is_err());
        let env = BTreeMap::from([("TOKEN".to_string(), "it's".to_string())]);
        assert_eq!(exports(env.iter()).unwrap(), "export TOKEN='it'\\''s'\n");
    }

    /// Always runs: a fake `limactl` logs its argv and runs `shell` commands
    /// on the host with an empty env, so the start args, shims, env file,
    /// gate exec and lifecycle are checked without a VM.
    #[tokio::test]
    async fn vm_shim_drives_the_backend_cli() {
        let (dir, mut spec) = fixture(ExecEnvSpec::Vm(spec("default")));
        let inner = dir.join("inner");
        std::fs::create_dir_all(&inner).unwrap();
        let log = dir.join("limactl.log");
        let limactl = dir.join("fake-limactl");
        script(&inner.join("claude"), FAKE_CLAUDE);
        script(
            &limactl,
            &format!(
                "#!/bin/sh\necho \"$*\" >> '{}'\n[ \"$1\" = shell ] || exit 0\nshift\n\
                 [ \"$1\" = --workdir ] && {{ cd \"$2\"; shift 2; }}\n\
                 shift\nexec env -i PATH='{}':/usr/bin:/bin \"$@\"\n",
                log.display(),
                inner.display()
            ),
        );
        let env = open(limactl, &dir, &spec);
        env.prepare(&mut spec).await.unwrap();
        let env_file = env.dir.join("session.env");
        let mode = std::fs::metadata(&env_file).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);

        // An adapter with an absolute binary path still gets the shim; the
        // token reaches the CLI only through the env file.
        run_claude(&shimmed(&spec), &spec).await;
        let out = env
            .exec(&gate(&spec, "echo $GATE_SECRET; pwd"))
            .await
            .unwrap();
        assert_eq!(out.code, Some(0), "{}", out.output);
        assert!(out.output.contains("[REDACTED]"), "{}", out.output);
        assert!(!out.output.contains("g4te-value-77"), "{}", out.output);
        assert!(out.output.contains(&*spec.worktree.to_string_lossy()));
        assert!(!env.dir.join("exec-1.sh").exists(), "exec script removed");
        env.teardown().await.unwrap();
        assert!(!env.dir.exists());

        let log = std::fs::read_to_string(&log).unwrap();
        let lines: Vec<&str> = log.lines().collect();
        let (name, wt) = (&env.name, spec.worktree.display());
        let mounts = json!([
            { "location": spec.worktree, "writable": true },
            { "location": dir.join("tmp/S"), "writable": true },
            { "location": env.dir, "writable": false },
        ]);
        assert_eq!(lines[0], format!("delete -f {name}"));
        assert_eq!(
            lines[1],
            format!(
                "start --name {name} --tty=false --set .mounts = {mounts} \
                 | .memory = \"2048MiB\" | .cpus = 2 template://default"
            )
        );
        assert_eq!(
            lines[2..],
            [
                format!(
                    "shell --workdir {wt} {name} sh -c . \"$0\" && exec claude \"$@\" {} -p",
                    env_file.display()
                ),
                format!(
                    "shell --workdir {wt} {name} sh {}",
                    env.dir.join("exec-1.sh").display()
                ),
                format!("delete -f {name}"),
            ]
        );
        assert!(
            !log.contains("s3cret") && !log.contains("g4te"),
            "secret in argv"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// The guest end of a `guestfwd=...-cmd:<cmd>`: runs `cmd` the way QEMU
    /// does for one guest connection, sends `request`, returns the reply.
    async fn through_guestfwd(cmd: &str, request: &str) -> String {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let mut child = Command::new("/bin/sh")
            .args(["-c", cmd])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let mut stdin = child.stdin.take().unwrap();
        stdin.write_all(request.as_bytes()).await.unwrap();
        let mut out = String::new();
        let mut stdout = child.stdout.take().unwrap();
        let read = stdout.read_to_string(&mut out);
        tokio::time::timeout(std::time::Duration::from_secs(10), read)
            .await
            .unwrap()
            .unwrap();
        drop(stdin);
        out
    }

    /// Always runs: a fake `limactl` starts the VM through the QEMU
    /// wrapper it is handed (a fake `qemu-system-<arch>` logs the final
    /// argv), so it checks that the allowlist lives on the host: the user
    /// netdev is `restrict=on` with one `guestfwd` to the host proxy, a
    /// firewall flush inside the VM changes nothing, the proxy tunnels only
    /// to allowed hosts, other network backends fail the start, root is
    /// kept, and `keep` stops instead of deleting.
    #[tokio::test]
    async fn vm_egress_is_enforced_on_the_host() {
        let mut vm = spec("default");
        vm.network = Some(vec!["localhost".into()]);
        vm.keep = true;
        let (dir, mut spec) = fixture(ExecEnvSpec::Vm(vm));
        let bin = dir.join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        let arch = std::env::consts::ARCH;
        let qemu_log = dir.join("qemu.log");
        script(
            &bin.join(format!("qemu-system-{arch}")),
            &format!(
                "#!/bin/sh\nfor a in \"$@\"; do echo \"$a\"; done > '{}'\n",
                qemu_log.display()
            ),
        );
        let fake = |netdev: &str| {
            format!(
                "#!/bin/sh\necho \"$*\" >> '{log}'\n\
                 [ \"$1\" = start ] && {{ PATH='{bin}':$PATH; eval \"$QEMU_SYSTEM_{key}\" \
                 -machine virt -netdev '{netdev}' -device virtio-net-pci,netdev=net0; exit $?; }}\n\
                 [ \"$1\" = list ] && echo '{inst}/'\"$4\"\n\
                 exit 0\n",
                log = dir.join("limactl.log").display(),
                inst = dir.join("lima").display(),
                bin = bin.display(),
                key = arch.to_ascii_uppercase(),
            )
        };
        let lima_netdev = "user,id=net0,net=192.168.5.0/24,hostfwd=tcp:127.0.0.1:60022-:22";
        let limactl = dir.join("fake-limactl");
        script(&limactl, &fake(lima_netdev));
        let env = open(limactl, &dir, &spec);
        env.prepare(&mut spec).await.unwrap();
        let instance = dir.join("lima").join(&env.name);
        std::fs::create_dir_all(&instance).unwrap();
        std::fs::write(instance.join("lima.yaml"), "").unwrap();

        let env_file = std::fs::read_to_string(env.dir.join("session.env")).unwrap();
        assert!(env_file.contains(&format!("export HTTPS_PROXY=http://{GUEST_PROXY}\n")));
        let argv = std::fs::read_to_string(&qemu_log).unwrap();
        let netdev = argv.lines().find(|l| l.starts_with("user,")).unwrap();
        let (head, cmd) = netdev.split_once("-cmd:").unwrap();
        assert_eq!(
            head,
            format!("{lima_netdev},restrict=on,guestfwd=tcp:{GUEST_PROXY}")
        );

        // Root inside the VM flushes every firewall it has: the filter is
        // QEMU's netdev and the host proxy, so nothing opens.
        let out = env.exec(&gate(&spec, "sudo nft flush ruleset")).await;
        assert_eq!(out.unwrap().code, Some(0));
        let upstream = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = upstream.local_addr().unwrap().port();
        tokio::spawn(async move {
            use tokio::io::AsyncWriteExt;
            let (mut socket, _) = upstream.accept().await.unwrap();
            socket.write_all(b"hello from upstream\n").await.unwrap();
        });
        for denied in ["example.com:443", "127.0.0.1:22", "localhost.evil.test:443"] {
            let reply = through_guestfwd(cmd, &format!("CONNECT {denied} HTTP/1.1\r\n\r\n")).await;
            assert!(reply.starts_with("HTTP/1.1 403"), "{denied}: {reply}");
        }
        let reply = through_guestfwd(
            cmd,
            &format!("GET http://localhost:{port}/ HTTP/1.1\r\n\r\n"),
        )
        .await;
        assert!(reply.starts_with("HTTP/1.1 405"), "{reply}");
        let reply =
            through_guestfwd(cmd, &format!("CONNECT localhost:{port} HTTP/1.1\r\n\r\n")).await;
        assert!(reply.starts_with("HTTP/1.1 200"), "{reply}");
        assert!(reply.ends_with("hello from upstream\n"), "{reply}");

        env.teardown().await.unwrap();
        let reply = Command::new("/bin/sh")
            .args(["-c", cmd])
            .output()
            .await
            .unwrap();
        assert!(reply.stdout.is_empty(), "proxy stopped at teardown");
        let log = std::fs::read_to_string(dir.join("limactl.log")).unwrap();
        let name = &env.name;
        assert!(
            log.contains(".vmType = \"qemu\" | .mountType = \"9p\""),
            "{log}"
        );
        assert!(log.lines().any(|l| l == format!("stop {name}")), "{log}");
        assert!(instance.join(LOCKED).is_file(), "kept and locked");
        // Deleted only by prepare's leftover cleanup, never at teardown.
        assert_eq!(log.matches("delete -f").count(), 1, "{log}");
        // The agent keeps root next to the allowlist (a
        // marketplace host also runs DROP_ROOT).
        assert!(!log.contains("drop_root.sh"), "{log}");
        assert!(DROP_ROOT.contains("gpasswd") && DROP_ROOT.contains("sudo still works"));

        // Any other network backend (or a netdev that already forwards)
        // fails the start: a Lima change can only fail closed.
        for netdev in [
            "socket,id=net0,fd=3",
            "vmnet-shared,id=net0",
            "user,id=net0,guestfwd=tcp:10.0.2.9:80-tcp:1.1.1.1:80",
            "user,id=net0,restrict=off",
        ] {
            let failing = dir.join("failing-limactl");
            script(&failing, &fake(netdev));
            let env = open(failing, &dir, &spec);
            assert!(env.prepare(&mut spec).await.is_err(), "{netdev}");
            env.teardown().await.unwrap();
        }

        for bad in [vec!["a b".to_string()], vec!["-x".into()], vec!["".into()]] {
            let bad = VmSpec {
                network: Some(bad),
                ..super::tests::spec("default")
            };
            let opened = Vm::new("limactl".into(), bad, SessionId::generate(), "/x".into());
            assert!(opened.is_err());
        }

        // A marketplace host runs only reset VMs with an allowlist.
        let mut config = crate::HostConfig::new(dir.join("data"));
        config.marketplace = true;
        let locked = VmSpec {
            network: Some(vec![]),
            ..super::tests::spec("default")
        };
        let id = SessionId::generate();
        assert!(super::super::open(&ExecEnvSpec::Vm(locked.clone()), id, &config).is_ok());
        let opened = Vm::new("limactl".into(), locked.clone(), id, "/x".into()).unwrap();
        assert!(!opened.drop_root, "root is kept off the marketplace");
        for refused in [
            ExecEnvSpec::Bare,
            ExecEnvSpec::Vm(super::tests::spec("default")),
            ExecEnvSpec::Vm(VmSpec {
                keep: true,
                ..locked
            }),
        ] {
            assert!(super::super::open(&refused, id, &config).is_err());
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// Always runs: a kept allowlisted VM is stopped and locked, so
    /// a hand `limactl start` (which would skip the QEMU wrapper and the
    /// host proxy) fails; a VM that can't be locked is deleted instead.
    #[tokio::test]
    async fn kept_vm_is_locked_against_a_hand_restart() {
        let mut vm = spec("default");
        vm.network = Some(vec!["localhost".into()]);
        vm.keep = true;
        let (dir, mut spec) = fixture(ExecEnvSpec::Vm(vm));
        let inst = dir.join("lima");
        let log = dir.join("limactl.log");
        // Lima, cut down: an instance is a dir with its `lima.yaml`.
        let limactl = dir.join("fake-limactl");
        script(
            &limactl,
            &format!(
                "#!/bin/sh\necho \"$*\" >> '{log}'\ncd '{inst}' || exit 1\n\
                 case \"$1\" in\n\
                 start) [ -f \"$3/lima.yaml\" ] || [ ! -d \"$3\" ] || {{ echo 'no lima.yaml' >&2; exit 1; }}\n\
                   mkdir -p \"$3\" && touch \"$3/lima.yaml\" ;;\n\
                 list) [ -d \"$4\" ] && echo \"$PWD/$4\" ;;\n\
                 delete) rm -rf \"$3\" ;;\n\
                 esac\nexit 0\n",
                log = log.display(),
                inst = inst.display(),
            ),
        );
        std::fs::create_dir_all(&inst).unwrap();
        let env = open(limactl.clone(), &dir, &spec);
        env.prepare(&mut spec).await.unwrap();
        let name = env.name.clone();
        assert!(inst.join(&name).join("lima.yaml").is_file());
        env.teardown().await.unwrap();

        assert!(!inst.join(&name).join("lima.yaml").exists());
        assert!(inst.join(&name).join(LOCKED).is_file(), "kept, locked");
        let text = std::fs::read_to_string(&log).unwrap();
        assert!(text.lines().any(|l| l == format!("stop {name}")), "{text}");
        assert_eq!(
            text.matches("delete -f").count(),
            1,
            "only prepare's cleanup"
        );
        // The hand restart fails closed.
        let restart = Command::new(&limactl)
            .args(["start", "--name", &name])
            .status()
            .await
            .unwrap();
        assert!(!restart.success());

        // Nothing to lock (Lima lost the instance): deleted, not kept open.
        let mut lost = spec.clone();
        lost.session_id = SessionId::generate();
        let env = open(limactl, &dir, &lost);
        env.teardown().await.unwrap();
        let text = std::fs::read_to_string(&log).unwrap();
        assert!(
            text.ends_with(&format!("delete -f {}\n", env.name)),
            "{text}"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// A real Lima VM: the CLI an adapter spawns runs inside it with the
    /// worktree, env and stdin; the agent has root; gates exec inside;
    /// teardown deletes it. Skips when `limactl` is not installed. The first
    /// run downloads the template's image.
    #[tokio::test]
    async fn vm_runs_the_agent_cli_inside() {
        if !succeeds("limactl", &["--version"], 30).await {
            eprintln!("skipped: limactl not installed");
            return;
        }
        let (dir, mut spec) = fixture(ExecEnvSpec::Vm(spec("default")));
        let env = open("limactl".into(), &dir, &spec);
        env.prepare(&mut spec).await.unwrap();

        let install = "sudo cp fake-claude /usr/local/bin/claude \
                       && sudo chmod +x /usr/local/bin/claude \
                       && echo $GATE_SECRET && sudo -n id -u && hostname";
        let out = env.exec(&gate(&spec, install)).await.unwrap();
        assert_eq!(out.code, Some(0), "{}", out.output);
        assert!(!out.output.contains("g4te-value-77"), "{}", out.output);
        assert!(out.output.lines().any(|l| l == "0"), "root: {}", out.output);
        let host = format!("lima-{}", env.name);
        assert!(out.output.lines().any(|l| l == host), "{}", out.output);

        let text = run_claude(&shimmed(&spec), &spec).await;
        assert!(text.lines().any(|l| l == format!("host={host}")), "{text}");

        env.teardown().await.unwrap();
        let list = Command::new("limactl").args(["list", "-q"]).output();
        let list = String::from_utf8_lossy(&list.await.unwrap().stdout).into_owned();
        assert!(!list.lines().any(|l| l == env.name), "{list}");
        env.teardown().await.unwrap(); // idempotent
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
