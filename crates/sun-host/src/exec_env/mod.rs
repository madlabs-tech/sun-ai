//! Execution environments: where a session's agent CLI and its
//! gate commands run. **Strategy** behind [`ExecEnv`], one instance per
//! session, picked by `SessionSpec::exec_env`:
//! - [`Bare`]: directly in the worktree on the host (the default);
//! - [`Container`]: in a docker/podman container with the worktree mounted;
//! - [`Vm`]: in a Lima VM with the worktree mounted;
//! - [`ModEnv`]: served by an extension (`provides.exec_envs`, mod API 1.2).
//!
//! Non-bare envs share one trick: `prepare` writes a shim per agent CLI into
//! the session's env dir that runs the same CLI inside the environment, puts
//! that dir first on `spec.env["PATH"]` and names it in
//! [`EXEC_SHIMS_ENV`], which `sun-runtime` adapters honour even for absolute
//! binary paths. The unchanged adapter thus runs the CLI inside.

mod bare;
mod container;
mod egress;
mod mod_env;
mod vm;

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;

use async_trait::async_trait;
use sun_core::host_protocol::{ExecEnvCapability, ExecOutput, ExecRequest};
use sun_core::{EXEC_SHIMS_ENV, Error, ExecEnvSpec, Result, SessionId, SessionSpec};
use tokio::process::Command;

pub use bare::Bare;
pub use container::Container;
pub use mod_env::{ModEnv, ModEnvs};
pub use vm::Vm;

use crate::HostConfig;
use crate::buffer::write_file;

/// One session's execution environment. Lifecycle: [`prepare`](Self::prepare)
/// once, then any number of [`exec`](Self::exec), then
/// [`teardown`](Self::teardown).
///
/// Contract for every impl:
/// - `prepare` brings the environment up and rewrites `spec` (env, paths) so
///   that the unchanged `RuntimeAdapter::start(spec)` runs the CLI inside it.
///   The worktree is visible inside at the same path as on the host, so paths
///   in events and requests mean the same on both sides.
/// - `exec` has the contract of the `exec` frame (`sun_core::host_protocol`):
///   `sh -c <command>` in `request.cwd`, timeout, redacted 64 KiB tail.
/// - `teardown` destroys everything `prepare` made and is idempotent; it is
///   also called after a failed `prepare`.
#[async_trait]
pub trait ExecEnv: Send + Sync {
    /// Brings the environment up for `spec`'s session.
    async fn prepare(&self, spec: &mut SessionSpec) -> Result<()>;

    /// Runs one command inside the environment.
    async fn exec(&self, request: &ExecRequest) -> Result<ExecOutput>;

    /// Destroys the environment.
    async fn teardown(&self) -> Result<()>;
}

/// The environment `spec` asks for, for session `session_id`. A
/// marketplace host ([`HostConfig::marketplace`]) refuses all but a reset
/// `vm` with a network allowlist.
pub fn open(
    spec: &ExecEnvSpec,
    session_id: SessionId,
    config: &HostConfig,
) -> Result<Arc<dyn ExecEnv>> {
    let isolated = matches!(spec, ExecEnvSpec::Vm(vm) if vm.network.is_some() && !vm.keep);
    if config.marketplace && !isolated {
        return Err(Error::InvalidInput(
            "a marketplace host runs tasks only in a `vm` with a network allowlist \
             and `keep: false`"
                .into(),
        ));
    }
    let dir = config.data_dir.join("env").join(session_id.to_string());
    Ok(match spec {
        ExecEnvSpec::Bare => Arc::new(Bare),
        ExecEnvSpec::Container(container) => Arc::new(Container::new(
            config.container_cli.clone(),
            container.clone(),
            session_id,
            dir,
        )?),
        ExecEnvSpec::Vm(vm) => {
            let mut vm = Vm::new(config.vm_cli.clone(), vm.clone(), session_id, dir)?;
            vm.drop_root = config.marketplace;
            Arc::new(vm)
        }
        ExecEnvSpec::Extension(ext) => {
            Arc::new(ModEnv::open(config.mod_envs.as_ref(), ext, session_id)?)
        }
    })
}

/// What `hello` reports: `bare` always; `container` when
/// `<container_cli> info` reaches its engine; `vm` when `<vm_cli> --version`
/// runs. The kernel offers only ready kinds.
pub(crate) async fn detect(config: &HostConfig) -> Vec<ExecEnvCapability> {
    let (container, vm) = tokio::join!(
        probe(
            &config.container_cli,
            &["info"],
            "install Docker or Podman and start its engine (`sun host run --container-cli podman` for Podman)",
        ),
        probe(
            &config.vm_cli,
            &["--version"],
            "install Lima (`brew install lima`, or your distro's package; needs KVM on Linux)",
        ),
    );
    let mut envs = vec![
        ExecEnvCapability {
            kind: "bare".into(),
            ready: true,
            version: None,
            detail: None,
        },
        capability("container", container),
        capability("vm", vm),
    ];
    let mods = config.mod_envs.iter().flat_map(|m| &m.names);
    envs.extend(mods.map(|name| capability(name, Ok(None))));
    envs
}

fn capability(
    kind: &str,
    probed: std::result::Result<Option<String>, String>,
) -> ExecEnvCapability {
    let (ready, version, detail) = match probed {
        Ok(version) => (true, version, None),
        Err(detail) => (false, None, Some(detail)),
    };
    ExecEnvCapability {
        kind: kind.into(),
        ready,
        version,
        detail,
    }
}

/// Runs `cli args` (5 s cap); on success the first line of `cli --version`.
async fn probe(
    cli: &Path,
    args: &[&str],
    fix: &str,
) -> std::result::Result<Option<String>, String> {
    let run = |args: Vec<&str>| {
        let mut cmd = Command::new(cli);
        cmd.args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        tokio::time::timeout(std::time::Duration::from_secs(5), cmd.output())
    };
    let name = cli.display();
    let failed = |why: String| format!("`{name} {}` {why}; {fix}", args.join(" "));
    match run(args.to_vec()).await {
        Err(_) => return Err(failed("timed out".into())),
        Ok(Err(e)) => return Err(failed(format!("cannot run: {e}"))),
        Ok(Ok(out)) if !out.status.success() => {
            let stderr = String::from_utf8_lossy(&out.stderr);
            let last = stderr
                .lines()
                .rev()
                .find(|l| !l.trim().is_empty())
                .unwrap_or("");
            return Err(failed(format!("failed: {}", last.trim())));
        }
        Ok(Ok(_)) => {}
    }
    let version = match run(vec!["--version"]).await {
        Ok(Ok(out)) if out.status.success() => String::from_utf8_lossy(&out.stdout)
            .lines()
            .next()
            .map(|l| l.trim().to_owned()),
        _ => None,
    };
    Ok(version)
}

/// CLIs that get a shim, by binary file name.
const AGENT_CLIS: &[&str] = &["claude", "codex", "opencode"];

/// Host paths the environment must see at the same path: the worktree, its
/// git common dir, the session `TMPDIR` and the MCP config.
fn mounts(spec: &SessionSpec) -> Vec<PathBuf> {
    let mut mounts = vec![spec.worktree.clone()];
    mounts.extend(git_common_dir(&spec.worktree));
    mounts.extend(spec.env.get("TMPDIR").map(PathBuf::from));
    mounts.extend(spec.mcp_config.clone());
    mounts
}

/// Names of the session env vars to forward into the environment.
fn forwarded(spec: &SessionSpec) -> Vec<String> {
    let skip = ["PATH", EXEC_SHIMS_ENV];
    let names = spec.env.keys().filter(|k| !skip.contains(&k.as_str()));
    names.cloned().collect()
}

/// Writes `shim(cli)` for every agent CLI into `dir` and points `spec` at it
/// (first on `PATH`, and [`EXEC_SHIMS_ENV`]).
fn install_shims(dir: &Path, spec: &mut SessionSpec, shim: impl Fn(&str) -> String) -> Result<()> {
    std::fs::create_dir_all(dir).map_err(io(dir))?;
    for cli in AGENT_CLIS {
        write_file(&dir.join(cli), &shim(cli), 0o755)?;
    }
    let base = spec
        .env
        .get("PATH")
        .cloned()
        .or_else(|| std::env::var("PATH").ok())
        .unwrap_or_default();
    let path = format!("{}:{base}", dir.display());
    spec.env.insert("PATH".into(), path);
    let dir = dir.to_string_lossy().into_owned();
    spec.env.insert(EXEC_SHIMS_ENV.into(), dir);
    Ok(())
}

/// Removes `dir`; a missing dir is fine (teardown is idempotent).
fn remove_dir(dir: &Path) -> Result<()> {
    match std::fs::remove_dir_all(dir) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(io(dir)(e)),
        _ => Ok(()),
    }
}

/// Runs a backend CLI (`docker`, `limactl`), returning stdout or the
/// trimmed stderr as `Unavailable`.
async fn run_cli(cli: &Path, args: &[String]) -> Result<String> {
    run_cli_env(cli, args, &[]).await
}

/// [`run_cli`] with extra env vars.
async fn run_cli_env(cli: &Path, args: &[String], env: &[(String, String)]) -> Result<String> {
    let output = Command::new(cli)
        .args(args)
        .envs(env.iter().map(|(k, v)| (k, v)))
        .stdin(Stdio::null())
        .output()
        .await
        .map_err(|e| Error::Unavailable(format!("cannot run {}: {e}", cli.display())))?;
    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    } else {
        Err(Error::Unavailable(format!(
            "{} {}: {}",
            cli.display(),
            args.first().map_or("", String::as_str),
            String::from_utf8_lossy(&output.stderr).trim()
        )))
    }
}

/// The shared git dir of a linked worktree (its `.git` file says
/// `gitdir: <repo>/worktrees/<name>`), which git inside the env needs.
fn git_common_dir(worktree: &Path) -> Option<PathBuf> {
    let text = std::fs::read_to_string(worktree.join(".git")).ok()?;
    let gitdir = PathBuf::from(text.strip_prefix("gitdir:")?.trim());
    let worktrees = gitdir.parent()?;
    (worktrees.file_name()? == "worktrees").then(|| worktrees.parent().map(Path::to_path_buf))?
}

fn sh_quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', r"'\''"))
}

fn io(path: &Path) -> impl FnOnce(std::io::Error) -> Error + '_ {
    move |e| Error::Unavailable(format!("{}: {e}", path.display()))
}

/// Fixtures shared by the env tests.
#[cfg(test)]
pub(crate) mod testing {
    #![allow(clippy::unwrap_used)]

    use std::collections::BTreeMap;

    use sun_core::{RunId, StepKey};

    use super::*;

    /// The "agent CLI": echoes what it got and writes a file in its cwd.
    pub const FAKE_CLAUDE: &str = "#!/bin/sh\necho \"args=$*\"\necho \"cwd=$(pwd)\"\n\
        echo \"token=$SUN_TOKEN\"\ntest -f /.dockerenv && echo inside\n\
        echo \"host=$(hostname)\"\nread line\necho \"stdin=$line\"\necho hi > out.txt\n";

    /// A session dir under the target dir (Docker Desktop and Lima can mount
    /// it) and a session spec for `exec_env` whose worktree holds
    /// `fake-claude`.
    pub fn fixture(exec_env: ExecEnvSpec) -> (PathBuf, SessionSpec) {
        let session_id = SessionId::generate();
        let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/sun-env-test")
            .join(session_id.to_string());
        std::fs::create_dir_all(dir.join("work/T")).unwrap();
        std::fs::create_dir_all(dir.join("tmp/S")).unwrap();
        let dir = dir.canonicalize().unwrap();
        let worktree = dir.join("work/T");
        std::fs::write(worktree.join("fake-claude"), FAKE_CLAUDE).unwrap();
        let spec = SessionSpec {
            skills: None,
            session_id,
            run_id: RunId::generate(),
            step_key: StepKey::new("implement").unwrap(),
            worktree,
            envelope: "do it".into(),
            envelope_parts: None,
            model: None,
            effort: None,
            mcp_config: None,
            env: BTreeMap::from([
                (
                    "TMPDIR".into(),
                    dir.join("tmp/S").to_string_lossy().into_owned(),
                ),
                ("SUN_TOKEN".into(), "s3cret-token-42".into()),
            ]),
            exec_env,
            resume: None,
            permission_mode: None,
            byo_key: None,
        };
        (dir, spec)
    }

    /// Writes an executable script.
    ///
    /// ponytail: rust-lang/rust#114554. A fork in another test thread can
    /// inherit a write fd we hold on the script until its exec closes it
    /// (O_CLOEXEC), and an exec in that window fails with "Text file busy".
    /// A `sh` child writes the file, so this process never holds that fd (a
    /// `--version` warm-up would run the fakes and add lines to the argv logs
    /// the tests assert on). Drop this once std closes the race.
    pub fn script(path: &Path, text: &str) {
        use std::io::Write;
        let mut sh = std::process::Command::new("sh")
            .args(["-c", "cat > \"$0\" && chmod 755 \"$0\""])
            .arg(path)
            .stdin(Stdio::piped())
            .spawn()
            .unwrap();
        sh.stdin.take().unwrap().write_all(text.as_bytes()).unwrap();
        assert!(sh.wait().unwrap().success(), "{}", path.display());
    }

    /// Spawns `program -p` the way the adapters do (cwd, `spec.env`) with one
    /// stdin line; checks and returns stdout.
    pub async fn run_claude(program: &Path, spec: &SessionSpec) -> String {
        use tokio::io::AsyncWriteExt;
        let mut child = Command::new(program)
            .arg("-p")
            .current_dir(&spec.worktree)
            .envs(&spec.env)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let mut stdin = child.stdin.take().unwrap();
        stdin.write_all(b"hello\n").await.unwrap();
        drop(stdin);
        let out = child.wait_with_output().await.unwrap();
        let text = String::from_utf8_lossy(&out.stdout).into_owned();
        assert!(out.status.success(), "{text}");
        let cwd = format!("cwd={}", spec.worktree.display());
        for line in ["args=-p", &cwd, "token=s3cret-token-42", "stdin=hello"] {
            assert!(
                text.lines().any(|l| l == line),
                "missing {line:?} in {text}"
            );
        }
        assert!(spec.worktree.join("out.txt").is_file());
        text
    }

    /// A gate command in the worktree with one secret env var.
    pub fn gate(spec: &SessionSpec, command: &str) -> ExecRequest {
        ExecRequest {
            cwd: spec.worktree.clone(),
            command: command.into(),
            env: BTreeMap::from([("GATE_SECRET".into(), "g4te-value-77".into())]),
            timeout_s: 600,
        }
    }

    /// `true` when `cli args…` exits 0 within `secs`.
    pub async fn succeeds(cli: &str, args: &[&str], secs: u64) -> bool {
        let status = Command::new(cli)
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .status();
        let status = tokio::time::timeout(std::time::Duration::from_secs(secs), status);
        status.await.is_ok_and(|s| s.is_ok_and(|s| s.success()))
    }
}

#[cfg(test)]
mod detect_tests {
    #![allow(clippy::unwrap_used)]

    use super::testing::script;
    use super::*;

    #[tokio::test]
    async fn detect_reports_ready_and_broken_backends() {
        let dir = std::env::temp_dir().join(format!("sun-detect-{}", SessionId::generate()));
        std::fs::create_dir_all(&dir).unwrap();
        let docker = dir.join("docker");
        script(
            &docker,
            "#!/bin/sh\n[ \"$1\" = --version ] && echo 'Docker version 28.5.1' && exit 0\n\
             echo 'Cannot connect to the Docker daemon' >&2; exit 1\n",
        );
        let lima = dir.join("limactl");
        script(&lima, "#!/bin/sh\necho 'limactl version 1.2.0'\n");
        let mut config = HostConfig::new(&dir);
        config.container_cli = docker;
        config.vm_cli = lima;

        let envs = detect(&config).await;
        let kinds: Vec<_> = envs.iter().map(|e| (e.kind.as_str(), e.ready)).collect();
        assert_eq!(kinds, [("bare", true), ("container", false), ("vm", true)]);
        let detail = envs[1].detail.as_deref().unwrap();
        assert!(
            detail.contains("Cannot connect to the Docker daemon"),
            "{detail}"
        );
        assert!(detail.contains("Podman"), "{detail}");
        assert_eq!(envs[2].version.as_deref(), Some("limactl version 1.2.0"));

        config.vm_cli = dir.join("missing");
        let envs = detect(&config).await;
        assert!(!envs[2].ready);
        assert!(envs[2].detail.as_deref().unwrap().contains("cannot run"));
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
