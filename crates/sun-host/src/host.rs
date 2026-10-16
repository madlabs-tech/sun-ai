//! The host's state: runtime adapters, live sessions, the event buffer and the
//! worktrees, plus the handler for every kernel request.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::time::{Duration, Instant};

use futures_util::StreamExt;
use sun_core::host_protocol::{
    Capabilities, DiskUse, HostFrame, KernelFrame, PreviewRequest, PreviewSide, ReplyBody,
    RuntimeCapability, SessionStats,
};
use sun_core::{
    Error, ExecEnvSpec, HostCtx, HostId, Result, RuntimeAdapter, SessionHandle, SessionId,
    SessionSpec, Steer, StopMode, SunEvent,
};

use crate::buffer::{self, Buffer};
use crate::device::{DeviceConfig, Devices};
use crate::exec_env::{self, ExecEnv};
use crate::preview::Previews;
use crate::process;
use crate::worktree::Worktrees;

/// Host settings. [`HostConfig::new`] has the defaults.
#[derive(Debug, Clone)]
pub struct HostConfig {
    /// Data dir: credentials, event buffer, bare clones, task dirs, temp dirs.
    pub data_dir: PathBuf,
    /// Cap of the on-disk event buffer in bytes.
    pub buffer_cap: u64,
    /// How often session RSS/CPU is sampled and sent.
    pub stats_every: Duration,
    /// First reconnect delay.
    pub backoff_min: Duration,
    /// Longest reconnect delay.
    pub backoff_max: Duration,
    /// Container CLI for `container` sessions: `docker` or `podman`.
    pub container_cli: PathBuf,
    /// Simulator, emulator and Maestro tools for `device.verify`.
    pub devices: DeviceConfig,
    /// Lima CLI for `vm` sessions.
    pub vm_cli: PathBuf,
    /// How long a container/VM stays up, unused, after its session ends, so
    /// the step's gates run in it; the next session in the same worktree or
    /// a `session.stop` releases it sooner.
    pub env_linger: Duration,
    /// Ecosystem skill store (`sun-catalog`), synced into each session's
    /// worktree. Default `<data>/skills`; a host on the kernel's machine
    /// points it at the kernel's `<data>/skills`.
    pub skills_dir: PathBuf,
    /// Serves marketplace tasks: every session must run in a
    /// `vm` with a network allowlist that is deleted afterwards
    /// (`keep: false`); anything else is refused.
    pub marketplace: bool,
    /// Extension exec envs (`kind: extension`); only a host in the kernel's
    /// process reaches mods. `None`: none.
    pub mod_envs: Option<exec_env::ModEnvs>,
    /// How often runtime readiness is probed again; a change goes to the
    /// kernel as a `capabilities` frame.
    pub reprobe_every: Duration,
    /// How often the host pings the kernel.
    pub ping_every: Duration,
    /// A connection that hears nothing (not even a pong) for this long is
    /// dead: the host reconnects.
    pub liveness: Duration,
}

impl HostConfig {
    /// Defaults: 256 MB buffer, stats every 5 s, back-off 1 s → 60 s,
    /// re-probe every 5 min, ping every 10 s, dead after 30 s of silence.
    pub fn new(data_dir: impl Into<PathBuf>) -> Self {
        let data_dir: PathBuf = data_dir.into();
        Self {
            skills_dir: data_dir.join("skills"),
            data_dir,
            buffer_cap: buffer::DEFAULT_CAP,
            stats_every: Duration::from_secs(5),
            backoff_min: Duration::from_secs(1),
            backoff_max: Duration::from_secs(60),
            container_cli: "docker".into(),
            devices: DeviceConfig::default(),
            vm_cli: "limactl".into(),
            env_linger: Duration::from_secs(600),
            marketplace: false,
            mod_envs: None,
            reprobe_every: Duration::from_secs(5 * 60),
            ping_every: Duration::from_secs(10),
            liveness: Duration::from_secs(30),
        }
    }
}

/// A runtime host: runs sessions for the kernel it is enrolled with.
pub struct Host {
    pub(crate) config: HostConfig,
    adapters: BTreeMap<String, Arc<dyn RuntimeAdapter>>,
    pub(crate) buffer: Buffer,
    worktrees: Worktrees,
    devices: Devices,
    previews: Previews,
    sessions: Mutex<HashMap<SessionId, Live>>,
    /// This host's id, once [`Host::run`] knows it (for readiness checks).
    pub(crate) host_id: OnceLock<HostId>,
    /// The capabilities last reported on this connection; held while a
    /// probe runs, so reports never go out of order.
    reported: tokio::sync::Mutex<Option<Capabilities>>,
}

struct Live {
    adapter: Arc<dyn RuntimeAdapter>,
    handle: SessionHandle,
    /// The session's env; `None` once released.
    env: Option<Arc<dyn ExecEnv>>,
    /// Last time an exec used `env`.
    env_used: Instant,
    tmp: PathBuf,
    worktree: PathBuf,
    started: Instant,
    alive: bool,
}

impl Host {
    /// Opens the host's data dir (resuming any buffered events) with the
    /// runtime adapters it offers. `sun-cli` injects the real adapters.
    pub fn open(config: HostConfig, adapters: Vec<Arc<dyn RuntimeAdapter>>) -> Result<Arc<Self>> {
        Ok(Arc::new(Self {
            buffer: Buffer::open(config.data_dir.join("buffer"), config.buffer_cap)?,
            worktrees: Worktrees::new(&config.data_dir)?,
            devices: Devices::new(config.devices.clone()),
            previews: Previews::new(config.data_dir.join("tmp")),
            adapters: adapters
                .into_iter()
                .map(|a| (a.id().to_owned(), a))
                .collect(),
            sessions: Mutex::new(HashMap::new()),
            host_id: OnceLock::new(),
            reported: tokio::sync::Mutex::new(None),
            config,
        }))
    }

    fn sessions(&self) -> MutexGuard<'_, HashMap<SessionId, Live>> {
        self.sessions.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// What this host can run. Every adapter is probed at once.
    async fn capabilities(&self, host_id: HostId) -> Capabilities {
        let ctx = HostCtx { host_id };
        let runtimes =
            futures_util::future::join_all(self.adapters.iter().map(|(id, adapter)| async {
                let (readiness, effort) =
                    tokio::join!(adapter.readiness(&ctx), adapter.effort(&ctx));
                RuntimeCapability {
                    adapter: id.clone(),
                    readiness,
                    effort,
                }
            }))
            .await;
        Capabilities {
            os: std::env::consts::OS.into(),
            arch: std::env::consts::ARCH.into(),
            cpus: std::thread::available_parallelism().map_or(1, |n| n.get() as u32),
            runtimes,
            devices: self.devices.detect().await,
            exec_envs: exec_env::detect(&self.config).await,
        }
    }

    /// The capabilities for a new connection's `hello`; later re-probes
    /// report against them.
    pub(crate) async fn hello_capabilities(&self, host_id: HostId) -> Capabilities {
        let mut reported = self.reported.lock().await;
        let capabilities = self.capabilities(host_id).await;
        *reported = Some(capabilities.clone());
        capabilities
    }

    /// Probes again and sends a `capabilities` frame if anything changed
    /// since the last report. A no-op before the first `hello`.
    pub(crate) async fn reprobe(&self) {
        let Some(&host_id) = self.host_id.get() else {
            return;
        };
        let mut reported = self.reported.lock().await;
        let capabilities = self.capabilities(host_id).await;
        if reported.as_ref().is_some_and(|r| *r != capabilities) {
            *reported = Some(capabilities.clone());
            self.buffer.send(HostFrame::Capabilities { capabilities });
        }
    }

    /// Handles one kernel frame. Requests run in their own task and answer
    /// with a `reply` on the current connection.
    pub(crate) fn dispatch(self: &Arc<Self>, frame: KernelFrame) {
        let id = match &frame {
            KernelFrame::Ack { session_id, seq } => {
                if let Err(e) = self.buffer.ack(*session_id, *seq) {
                    eprintln!("sun-host: ack {session_id}: {e}");
                }
                return;
            }
            // Only valid as the answer on a one-shot connection.
            KernelFrame::Enrolled { .. } | KernelFrame::Status { .. } | KernelFrame::Left => return,
            KernelFrame::WorktreePrepare { id, .. }
            | KernelFrame::WorktreeRemove { id, .. }
            | KernelFrame::SessionStart { id, .. }
            | KernelFrame::SessionSend { id, .. }
            | KernelFrame::SessionStop { id, .. }
            | KernelFrame::Probe { id, .. }
            | KernelFrame::Exec { id, .. }
            | KernelFrame::WorktreeWrite { id, .. }
            | KernelFrame::DeviceVerify { id, .. }
            | KernelFrame::DeviceScreen { id, .. }
            | KernelFrame::DeviceVideo { id, .. }
            | KernelFrame::DeviceInput { id, .. }
            | KernelFrame::DeviceFlow { id, .. }
            | KernelFrame::PreviewStart { id, .. }
            | KernelFrame::PreviewHttp { id, .. }
            | KernelFrame::PreviewScreenshot { id, .. }
            | KernelFrame::PreviewStop { id, .. }
            | KernelFrame::Reprobe { id } => *id,
        };
        let host = Arc::clone(self);
        tokio::spawn(async move {
            let body = host
                .request(frame)
                .await
                .unwrap_or_else(|e| ReplyBody::from(&e));
            host.buffer.send(HostFrame::Reply { id, body });
        });
    }

    async fn request(self: &Arc<Self>, frame: KernelFrame) -> Result<ReplyBody> {
        Ok(match frame {
            KernelFrame::WorktreePrepare { worktree, .. } => ReplyBody::Worktree {
                path: self.worktrees.prepare(&worktree).await?,
            },
            KernelFrame::SessionStart { adapter, spec, .. } => ReplyBody::Session {
                handle: self.start(&adapter, *spec).await?,
            },
            KernelFrame::SessionSend {
                session_id, text, ..
            } => {
                let (adapter, handle) = self
                    .live(session_id)
                    .ok_or_else(|| Error::NotFound(format!("session {session_id}")))?;
                adapter.send(&handle, Steer { text }).await?;
                ReplyBody::Done
            }
            KernelFrame::SessionStop {
                session_id, how, ..
            } => {
                // Unknown session: already gone, and stop is idempotent.
                if !self.sessions().get(&session_id).is_some_and(|l| l.alive) {
                    self.release(session_id).await;
                } else if let Some((adapter, handle)) = self.live(session_id) {
                    // Adapters spawn their CLI as a process-group leader.
                    if let (StopMode::Kill, Some(pid)) = (how, handle.pid) {
                        process::kill_group(pid).await;
                    }
                    adapter.stop(&handle, how).await?;
                }
                ReplyBody::Done
            }
            KernelFrame::Probe { session_id, .. } => ReplyBody::Alive {
                alive: self.sessions().get(&session_id).is_some_and(|l| l.alive),
            },
            KernelFrame::Exec { mut request, .. } => {
                request.cwd = self.worktrees.check(&request.cwd)?;
                let output = self.use_env(&request.cwd).exec(&request).await;
                self.use_env(&request.cwd); // the linger counts from the end
                ReplyBody::Exec { output: output? }
            }
            KernelFrame::WorktreeRemove { task_id, .. } => {
                self.previews.stop(task_id).await;
                self.devices.release(&task_id.to_string()).await;
                self.worktrees.remove(task_id).await?;
                ReplyBody::Done
            }
            KernelFrame::WorktreeWrite { cwd, files, .. } => {
                crate::worktree::write_files(&self.worktrees.check(&cwd)?, &files)?;
                ReplyBody::Done
            }
            KernelFrame::DeviceVerify { mut request, .. } => {
                request.cwd = self.worktrees.check(&request.cwd)?;
                let scratch = self.config.data_dir.join("tmp");
                ReplyBody::Device {
                    result: self.devices.verify(&scratch, &request).await?,
                }
            }
            KernelFrame::DeviceScreen { task_id, .. } => ReplyBody::Screen {
                screen: self.devices.screen(&task_id.to_string()).await?,
            },
            KernelFrame::DeviceVideo {
                task_id, stream, ..
            } => ReplyBody::Video {
                video: self.devices.video(&task_id.to_string(), &stream).await?,
            },
            KernelFrame::DeviceInput { task_id, input, .. } => {
                self.devices.input(&task_id.to_string(), &input).await?;
                ReplyBody::Done
            }
            KernelFrame::PreviewStart { request, .. } => {
                self.start_preview(&request).await?;
                ReplyBody::Done
            }
            KernelFrame::PreviewScreenshot { request, .. } => {
                let task_id = request.worktree.task_id();
                // No canvas open: serve at the root until one starts it under its base.
                if !self.previews.is_running(task_id, request.side).await {
                    let start = PreviewRequest {
                        worktree: request.worktree.clone(),
                        side: request.side,
                        base_path: "/".into(),
                        timeout_s: 300,
                    };
                    self.start_preview(&start).await?;
                }
                ReplyBody::Png {
                    png: self.previews.screenshot(task_id, &request).await?,
                }
            }
            KernelFrame::PreviewHttp {
                task_id,
                side,
                request,
                ..
            } => ReplyBody::Http {
                response: self.previews.http(task_id, side, &request).await?,
            },
            KernelFrame::PreviewStop { task_id, .. } => {
                self.previews.stop(task_id).await;
                ReplyBody::Done
            }
            KernelFrame::Reprobe { .. } => {
                self.reprobe().await;
                ReplyBody::Done
            }
            KernelFrame::DeviceFlow { mut request, .. } => {
                request.cwd = self.worktrees.check(&request.cwd)?;
                let scratch = self.config.data_dir.join("tmp");
                ReplyBody::Device {
                    result: self.devices.flow(&scratch, &request).await?,
                }
            }
            KernelFrame::Ack { .. }
            | KernelFrame::Enrolled { .. }
            | KernelFrame::Status { .. }
            | KernelFrame::Left => {
                return Err(Error::Protocol("not a request".into()));
            }
        })
    }

    /// Prepares the side's checkout and starts (or reuses) its dev server.
    async fn start_preview(&self, request: &PreviewRequest) -> Result<()> {
        let task_id = request.worktree.task_id();
        let worktree = self.worktrees.prepare(&request.worktree).await?;
        let dir = match request.side {
            PreviewSide::After => worktree.clone(),
            PreviewSide::Before => self.worktrees.prepare_base(&request.worktree).await?,
        };
        self.previews.start(task_id, &worktree, &dir, request).await
    }

    fn live(&self, session: SessionId) -> Option<(Arc<dyn RuntimeAdapter>, SessionHandle)> {
        self.sessions()
            .get(&session)
            .map(|l| (Arc::clone(&l.adapter), l.handle.clone()))
    }

    /// Exec commands in a worktree run in the env of its session (live or
    /// lingering, see [`HostConfig::env_linger`]), so gates use the session's
    /// toolchain; otherwise bare. Marks that env used.
    fn use_env(&self, cwd: &Path) -> Arc<dyn ExecEnv> {
        let mut sessions = self.sessions();
        let env = sessions.values_mut().find_map(|l| {
            let env = l.env.clone().filter(|_| l.worktree == cwd)?;
            l.env_used = Instant::now();
            Some(env)
        });
        env.unwrap_or_else(|| Arc::new(exec_env::Bare))
    }

    /// Tears down a session's env (if not yet) and removes its `TMPDIR`.
    async fn release(&self, id: SessionId) {
        let taken = self
            .sessions()
            .get_mut(&id)
            .and_then(|l| Some((l.env.take()?, l.tmp.clone())));
        if let Some((env, tmp)) = taken {
            teardown(&*env, id).await;
            let _ = tokio::fs::remove_dir_all(&tmp).await;
        }
    }

    /// After `id` ended: releases its env once it is unused for
    /// `env_linger` and no exec is running in it.
    async fn reap(&self, id: SessionId) {
        let linger = self.config.env_linger;
        loop {
            tokio::time::sleep(linger).await;
            let idle = match self.sessions().get(&id) {
                // Only the session holds the env: no exec in flight.
                Some(Live {
                    env: Some(env),
                    env_used,
                    ..
                }) => Arc::strong_count(env) == 1 && env_used.elapsed() >= linger,
                _ => return,
            };
            if idle {
                return self.release(id).await;
            }
        }
    }

    async fn start(
        self: &Arc<Self>,
        adapter_id: &str,
        mut spec: SessionSpec,
    ) -> Result<SessionHandle> {
        let adapter = self
            .adapters
            .get(adapter_id)
            .cloned()
            .ok_or_else(|| Error::NotFound(format!("runtime adapter `{adapter_id}`")))?;
        spec.worktree = self.worktrees.check(&spec.worktree)?;
        let id = spec.session_id;
        if self.sessions().contains_key(&id) {
            return Err(Error::InvalidInput(format!("session {id} already exists")));
        }
        // The workspace's installed skills go into the CLI's native folder.
        let skills = spec.skills.as_deref();
        sun_runtime::skills::sync(&self.config.skills_dir, &spec.worktree, adapter_id, skills)?;
        let tmp = self.config.data_dir.join("tmp").join(id.to_string());
        std::fs::create_dir_all(&tmp).map_err(buffer::io(&tmp))?;
        spec.env
            .insert("TMPDIR".into(), tmp.to_string_lossy().into_owned());

        // An env lingering in this worktree belongs to the previous step.
        let lingering: Vec<SessionId> = self
            .sessions()
            .iter()
            .filter(|(_, l)| !l.alive && l.env.is_some() && l.worktree == spec.worktree)
            .map(|(id, _)| *id)
            .collect();
        for old in lingering {
            self.release(old).await;
        }
        // Before `prepare`, so a container or VM gets the provider vars too.
        sun_runtime::byo_key::native_env(&mut spec, adapter_id)?;
        let bare = spec.exec_env == ExecEnvSpec::Bare;
        let env = exec_env::open(&spec.exec_env, id, &self.config)?;
        let worktree = spec.worktree.clone();
        let prepared = match env.prepare(&mut spec).await {
            Ok(()) => adapter.start(spec).await,
            Err(e) => Err(e),
        };
        let handle = match prepared {
            Ok(handle) => handle,
            Err(e) => {
                teardown(&*env, id).await;
                let _ = std::fs::remove_dir_all(&tmp);
                return Err(e);
            }
        };
        let mut events = adapter.events(&handle);
        self.buffer.begin(id);
        self.sessions().insert(
            id,
            Live {
                adapter,
                handle: handle.clone(),
                env: Some(env),
                env_used: Instant::now(),
                tmp,
                worktree,
                started: Instant::now(),
                alive: true,
            },
        );
        let host = Arc::clone(self);
        tokio::spawn(async move {
            let mut auth_failed = false;
            while let Some(event) = events.next().await {
                if let SunEvent::Error { message } = &event {
                    auth_failed |= sun_runtime::auth_error::is_auth_error(message);
                }
                if let Err(e) = host.buffer.append(id, event) {
                    eprintln!("sun-host: buffer {id}: {e}");
                }
            }
            host.buffer.finish(id);
            // The login went bad: show it now, not at the next 5-min probe.
            if auth_failed {
                let host = Arc::clone(&host);
                tokio::spawn(async move { host.reprobe().await });
            }
            let worktree = host.sessions().get_mut(&id).map(|live| {
                live.alive = false;
                live.env_used = Instant::now();
                live.worktree.clone()
            });
            // The device the agent booted ends with its step.
            // ponytail: per task, so parallel steps of one task share it and
            // the first to end shuts it down; key by session if that bites.
            if let Some(worktree) = worktree {
                host.devices
                    .release(&crate::device::task_of(&worktree))
                    .await;
            }
            if bare {
                host.release(id).await;
            } else {
                host.reap(id).await;
            }
        });
        Ok(handle)
    }

    /// Sends one `disk` frame: bytes per task folder.
    pub(crate) async fn sample_disk(&self) {
        let tasks = self.worktrees.disk_use().await;
        self.buffer.send(HostFrame::Disk(DiskUse { tasks }));
    }

    /// Sends one `stats` frame per live session (RSS/CPU of its process tree).
    // ponytail: a container session's tree is the docker client only; read
    // `docker stats` when container RSS/CPU must count against the concurrency caps.
    pub(crate) async fn sample_stats(&self) {
        let live: Vec<(SessionId, u32, Instant)> = self
            .sessions()
            .iter()
            .filter(|(_, l)| l.alive)
            .filter_map(|(id, l)| Some((*id, l.handle.pid?, l.started)))
            .collect();
        if live.is_empty() {
            return;
        }
        let procs = process::snapshot().await;
        for (session_id, pid, started) in live {
            let (rss_bytes, cpu_percent) = process::usage(&process::tree(&procs, pid));
            self.buffer.send(HostFrame::Stats(SessionStats {
                session_id,
                rss_bytes,
                cpu_percent,
                uptime_s: started.elapsed().as_secs(),
            }));
        }
    }
}

/// Tears a session's env down, logging (not failing on) errors.
async fn teardown(env: &dyn ExecEnv, session: SessionId) {
    if let Err(e) = env.teardown().await {
        eprintln!("sun-host: teardown {session}: {e}");
    }
}
