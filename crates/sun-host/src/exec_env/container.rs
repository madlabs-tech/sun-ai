//! The `container` environment: one docker/podman container per session.
//!
//! `prepare` starts `sun-<session_id>` (idle `tail -f /dev/null`, `--init`)
//! with the worktree, its git dir, the session `TMPDIR` and the MCP config
//! bind-mounted at the same paths as on the host, plus the resource caps. It
//! runs as the worktree's owner (`--user uid:gid`, `HOME=$TMPDIR`), so files
//! the agent writes stay the host user's on Linux. It then writes one shim
//! per agent CLI (see [`super`]) that `exec`s `docker exec -i` of the same
//! CLI inside the container: stdin, stdout, exit code and `kill` of the
//! client behave like the bare CLI. Env values reach the container as
//! `-e NAME` (taken from the client's environment), so secrets never appear
//! in an argv.

use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use async_trait::async_trait;
use sun_core::host_protocol::{ExecOutput, ExecRequest};
use sun_core::{ContainerSpec, Error, ExecEnvSpec, Result, SessionId, SessionSpec};
use tokio::process::Command;

use super::{ExecEnv, forwarded, install_shims, io, mounts, remove_dir, run_cli, sh_quote};
use crate::process;

/// A session's container (docker or podman CLI).
pub struct Container {
    cli: PathBuf,
    spec: ContainerSpec,
    session_id: SessionId,
    name: String,
    shim_dir: PathBuf,
}

impl Container {
    /// Validates `spec`; nothing starts until [`ExecEnv::prepare`].
    pub fn new(
        cli: PathBuf,
        spec: ContainerSpec,
        session_id: SessionId,
        shim_dir: PathBuf,
    ) -> Result<Self> {
        ExecEnvSpec::Container(spec.clone()).validate()?;
        Ok(Self {
            cli,
            spec,
            session_id,
            name: format!("sun-{session_id}"),
            shim_dir,
        })
    }

    /// `docker run` arguments for a container with `mounts` and `workdir`,
    /// running as `user` (`uid:gid`) with `HOME=home`.
    fn run_args(
        &self,
        mounts: &[PathBuf],
        workdir: &Path,
        user: &str,
        home: Option<&String>,
    ) -> Result<Vec<String>> {
        let mut args: Vec<String> = ["run", "-d", "--init", "--name", &self.name, "--label"]
            .map(String::from)
            .into();
        args.push(format!("sun.session={}", self.session_id));
        for mount in mounts {
            let path = mount.to_str().filter(|p| !p.contains([':', ',']));
            let path = path.ok_or_else(|| {
                Error::InvalidInput(format!(
                    "cannot mount {}: needs a plain path",
                    mount.display()
                ))
            })?;
            args.extend(["-v".into(), format!("{path}:{path}")]);
        }
        args.extend(["-w".into(), workdir.to_string_lossy().into_owned()]);
        args.extend(["--user".into(), user.into()]);
        if let Some(home) = home {
            args.extend(["-e".into(), format!("HOME={home}")]);
        }
        if let Some(mb) = self.spec.memory_mb {
            args.extend(["--memory".into(), format!("{mb}m")]);
        }
        if let Some(milli) = self.spec.cpu_millis {
            args.extend([
                "--cpus".into(),
                format!("{}.{:03}", milli / 1000, milli % 1000),
            ]);
        }
        if let Some(network) = &self.spec.network {
            args.extend(["--network".into(), network.clone()]);
        }
        args.extend(
            ["--entrypoint", "tail", &self.spec.image, "-f", "/dev/null"].map(String::from),
        );
        Ok(args)
    }

    /// The shim that runs `cli` inside the container.
    fn shim(&self, cli: &str, workdir: &Path, env_names: &[String]) -> String {
        let mut line = format!(
            "exec {} exec -i -w {}",
            sh_quote(&self.cli.to_string_lossy()),
            sh_quote(&workdir.to_string_lossy())
        );
        for name in env_names {
            line.push_str(&format!(" -e {}", sh_quote(name)));
        }
        line.push_str(&format!(" {} {cli} \"$@\"", sh_quote(&self.name)));
        format!("#!/bin/sh\n{line}\n")
    }

    /// `docker rm -f` of this session's container.
    async fn remove(&self) -> Result<String> {
        let args = ["rm".into(), "-f".into(), self.name.clone()];
        run_cli(&self.cli, &args).await
    }
}

#[async_trait]
impl ExecEnv for Container {
    async fn prepare(&self, spec: &mut SessionSpec) -> Result<()> {
        let owner = std::fs::metadata(&spec.worktree).map_err(io(&spec.worktree))?;
        let user = format!("{}:{}", owner.uid(), owner.gid());
        let home = spec.env.get("TMPDIR");
        let run = self.run_args(&mounts(spec), &spec.worktree, &user, home)?;
        // A container left over from a crashed host would block the name.
        let _ = self.remove().await;
        run_cli(&self.cli, &run).await?;

        let names = forwarded(spec);
        let worktree = spec.worktree.clone();
        install_shims(&self.shim_dir, spec, |cli| {
            self.shim(cli, &worktree, &names)
        })
    }

    async fn exec(&self, request: &ExecRequest) -> Result<ExecOutput> {
        let mut cmd = Command::new(&self.cli);
        cmd.args(["exec", "-w"]).arg(&request.cwd);
        for name in request.env.keys() {
            cmd.arg("-e").arg(name);
        }
        cmd.arg(&self.name)
            .args(["sh", "-c", &request.command])
            .envs(&request.env);
        // ponytail: a timeout kills the docker client, not the command in the
        // container; it dies at teardown. Wrap in `timeout` if gates hang.
        process::run(cmd, request).await
    }

    async fn teardown(&self) -> Result<()> {
        let removed = self.remove().await;
        remove_dir(&self.shim_dir)?;
        match removed {
            Err(Error::Unavailable(e)) if e.contains("No such container") => Ok(()),
            other => other.map(drop),
        }
    }
}

/// Rejects values that would turn into docker options (`-v/:/x`) or worse.
#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use sun_core::ExecEnvSpec;

    use super::super::git_common_dir;
    use super::super::testing::{FAKE_CLAUDE, fixture, gate, run_claude, script, succeeds};
    use super::*;

    fn spec(image: &str, network: Option<&str>) -> ContainerSpec {
        ContainerSpec {
            image: image.into(),
            memory_mb: Some(256),
            cpu_millis: Some(1500),
            network: network.map(Into::into),
        }
    }

    fn container(spec: ContainerSpec, shim_dir: PathBuf) -> Result<Container> {
        Container::new("docker".into(), spec, SessionId::generate(), shim_dir)
    }

    #[test]
    fn container_spec_rejects_option_injection() {
        for good in [
            "alpine",
            "alpine:3.20",
            "ghcr.io/acme/tool-chain:1.4",
            "a/b@sha256:ab12",
        ] {
            assert!(
                container(spec(good, Some("none")), "/x".into()).is_ok(),
                "{good}"
            );
        }
        for bad in ["", "-v/:/x", "--privileged", "a b", "a;rm", "/abs"] {
            assert!(container(spec(bad, None), "/x".into()).is_err(), "{bad:?}");
        }
        assert!(container(spec("alpine", Some("--network=host")), "/x".into()).is_err());
        let zero = ContainerSpec {
            memory_mb: Some(0),
            ..spec("alpine", None)
        };
        assert!(container(zero, "/x".into()).is_err());
        // The wire form the kernel sends.
        let wire: ExecEnvSpec =
            serde_json::from_str(r#"{"kind":"container","image":"alpine"}"#).unwrap();
        assert_eq!(
            wire,
            ExecEnvSpec::Container(ContainerSpec {
                image: "alpine".into(),
                memory_mb: None,
                cpu_millis: None,
                network: None
            })
        );
    }

    #[test]
    fn container_run_args_and_shim() {
        let env = container(spec("alpine", Some("none")), "/x".into()).unwrap();
        let wt = PathBuf::from("/data/work/T");
        let tmp = "/data/tmp/S".to_string();
        let args = env
            .run_args(&[wt.clone(), tmp.clone().into()], &wt, "501:20", Some(&tmp))
            .unwrap();
        let joined = args.join(" ");
        assert!(joined.starts_with(&format!("run -d --init --name {} --label", env.name)));
        assert!(joined.contains(
            "-v /data/work/T:/data/work/T -v /data/tmp/S:/data/tmp/S -w /data/work/T --user 501:20 -e HOME=/data/tmp/S --memory 256m --cpus 1.500 --network none --entrypoint tail alpine -f /dev/null"
        ));
        assert!(env.run_args(&["/a:b".into()], &wt, "1:1", None).is_err());

        let shim = env.shim("claude", Path::new("/it's"), &["TOKEN".to_string()]);
        assert_eq!(
            shim,
            format!(
                "#!/bin/sh\nexec 'docker' exec -i -w '/it'\\''s' -e 'TOKEN' '{}' claude \"$@\"\n",
                env.name
            )
        );
    }

    #[test]
    fn container_finds_the_git_common_dir() {
        let dir = std::env::temp_dir().join(format!("sun-gitdir-{}", SessionId::generate()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(".git"), "gitdir: /d/repos/x.git/worktrees/T\n").unwrap();
        assert_eq!(git_common_dir(&dir), Some(PathBuf::from("/d/repos/x.git")));
        std::fs::write(dir.join(".git"), "gitdir: /d/elsewhere\n").unwrap();
        assert_eq!(git_common_dir(&dir), None);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// Image for the real-container test; must have `sh`, `tail`, `cp`.
    const IMAGE: &str = "alpine:3.20";

    fn open(cli: PathBuf, dir: &Path, spec: &SessionSpec) -> Container {
        let ExecEnvSpec::Container(container) = &spec.exec_env else {
            panic!()
        };
        Container::new(cli, container.clone(), spec.session_id, dir.join("env")).unwrap()
    }

    /// Always runs: a fake engine CLI logs its argv and runs `exec` on the
    /// host, so the shim, env forwarding and lifecycle are checked without
    /// docker.
    #[tokio::test]
    async fn container_shim_drives_the_engine_cli() {
        let (dir, mut spec) = fixture(ExecEnvSpec::Container(spec(IMAGE, Some("none"))));
        let inner = dir.join("inner");
        std::fs::create_dir_all(&inner).unwrap();
        let log = dir.join("engine.log");
        let engine = dir.join("fake-docker");
        script(&inner.join("claude"), FAKE_CLAUDE);
        script(
            &engine,
            &format!(
                "#!/bin/sh\necho \"$*\" >> '{}'\n[ \"$1\" = exec ] || exit 0\nshift\n\
                 while :; do case \"$1\" in -i) shift;; -w) cd \"$2\"; shift 2;; \
                 -e) shift 2;; *) break;; esac; done\n\
                 shift\nPATH='{}':/usr/bin:/bin exec \"$@\"\n",
                log.display(),
                inner.display()
            ),
        );
        let env = open(engine, &dir, &spec);
        env.prepare(&mut spec).await.unwrap();
        // Adapters with a bare binary name find the shim on PATH.
        run_claude(Path::new("claude"), &spec).await;
        let out = env
            .exec(&gate(&spec, "echo $GATE_SECRET; pwd"))
            .await
            .unwrap();
        assert_eq!(out.code, Some(0));
        assert!(!out.output.contains("g4te-value-77"), "{}", out.output);
        assert!(out.output.contains(&*spec.worktree.to_string_lossy()));
        env.teardown().await.unwrap();
        assert!(!env.shim_dir.exists());

        let log = std::fs::read_to_string(&log).unwrap();
        let lines: Vec<&str> = log.lines().collect();
        let (name, wt) = (&env.name, spec.worktree.display());
        let tmp = dir.join("tmp/S");
        let owner = std::fs::metadata(&spec.worktree).unwrap();
        assert_eq!(lines[0], format!("rm -f {name}"));
        assert!(lines[1].starts_with(&format!("run -d --init --name {name}")));
        assert!(lines[1].contains(&format!(
            "-v {wt}:{wt} -v {0}:{0} -w {wt} --user {1}:{2} -e HOME={0} --memory 256m",
            tmp.display(),
            owner.uid(),
            owner.gid()
        )));
        assert_eq!(
            lines[2..],
            [
                format!("exec -i -w {wt} -e SUN_TOKEN -e TMPDIR {name} claude -p"),
                format!("exec -w {wt} -e GATE_SECRET {name} sh -c echo $GATE_SECRET; pwd"),
                format!("rm -f {name}"),
            ]
        );
        assert!(
            !log.contains("s3cret") && !log.contains("g4te"),
            "secret in argv"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// `true` when a docker engine answers and [`IMAGE`] is (or can be) pulled.
    async fn docker_ready() -> bool {
        succeeds("docker", &["info"], 120).await
            && (succeeds("docker", &["image", "inspect", IMAGE], 120).await
                || succeeds("docker", &["pull", "-q", IMAGE], 120).await)
    }

    async fn inspect(name: &str) -> Option<String> {
        let format = "{{.HostConfig.Memory}} {{.HostConfig.NanoCpus}} {{.HostConfig.NetworkMode}}";
        let out = Command::new("docker")
            .args(["inspect", "-f", format, name])
            .output()
            .await
            .unwrap();
        out.status
            .success()
            .then(|| String::from_utf8_lossy(&out.stdout).trim().to_owned())
    }

    /// A real container: the CLI an adapter spawns runs inside it, with the
    /// worktree, env, stdin and caps, as the worktree's owner; gates exec
    /// inside; teardown removes it. Skips when no docker engine answers.
    #[tokio::test]
    async fn container_runs_the_agent_cli_inside() {
        if !docker_ready().await {
            eprintln!("skipped: no docker engine or {IMAGE} image");
            return;
        }
        let (dir, mut spec) = fixture(ExecEnvSpec::Container(spec(IMAGE, Some("none"))));
        let env = open("docker".into(), &dir, &spec);
        env.prepare(&mut spec).await.unwrap();

        // Install the "CLI" into the image's PATH as root (the session itself
        // runs as the worktree's owner).
        let install = "cp fake-claude /usr/local/bin/claude && chmod +x /usr/local/bin/claude";
        let args = ["exec", "-u", "0", &env.name, "sh", "-c", install];
        assert!(succeeds("docker", &args, 60).await);

        let gate_cmd = "echo $GATE_SECRET && test -f /.dockerenv && echo inside && id -u";
        let out = env.exec(&gate(&spec, gate_cmd)).await.unwrap();
        assert_eq!(out.code, Some(0), "{}", out.output);
        assert!(out.output.contains("inside") && !out.output.contains("g4te-value-77"));
        let uid = std::fs::metadata(&spec.worktree).unwrap().uid();
        assert!(
            out.output.lines().any(|l| l == uid.to_string()),
            "{}",
            out.output
        );

        assert!(
            run_claude(Path::new("claude"), &spec)
                .await
                .lines()
                .any(|l| l == "inside")
        );
        let written = std::fs::metadata(spec.worktree.join("out.txt")).unwrap();
        assert_eq!(written.uid(), uid, "agent files stay the host user's");
        assert_eq!(
            inspect(&env.name).await.as_deref(),
            Some("268435456 1500000000 none")
        );

        env.teardown().await.unwrap();
        assert_eq!(inspect(&env.name).await, None);
        env.teardown().await.unwrap(); // idempotent
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
