//! Device verify: find the simulators, emulators and Maestro on this
//! machine, and run one verify on a device instance of its own.
//!
//! **Pattern:** **Strategy** over the device family. A [`Device`] is an iOS
//! Simulator (`xcrun simctl`) or an Android emulator (`emulator` + `adb`).
//! Each boots its own instance, installs the app, takes the final screenshot
//! and is torn down; Maestro runs the flow on either.
//!
//! A verify: check Maestro → pick the device → build → boot `sun-<session>` →
//! install → `maestro --device <id> test <flow>` → final screenshot →
//! shut down and delete. Teardown runs whatever happened. A failing build,
//! install or flow is a result (`passed: false`); missing tools or a device
//! that won't boot is an error.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use serde_json::Value;
use sun_core::host_protocol::{
    DeviceCapabilities, DeviceFlowRequest, DeviceInput, DeviceKey, DevicePlatform, DeviceRequest,
    DeviceResult, DeviceScreen, DeviceTool, DeviceVideo, ExecOutput, ExecRequest, Screenshot,
    VideoFrame,
};
use sun_core::{Error, Result, SessionId};
use tokio::process::{Child, Command};
use tokio::sync::{OnceCell, Semaphore};

use crate::process;

mod video;

/// A tool probe (`--version`, list devices) gives up after this.
const PROBE_TIMEOUT: Duration = Duration::from_secs(30);
/// Name of the screenshot taken after the flow.
const FINAL_SHOT: &str = "sun-final.png";
/// Screenshots kept per verify (the last ones, so the final screen stays).
const MAX_SHOTS: usize = 20;
/// Bytes of screenshots per verify; keeps the reply frame well under the
/// WebSocket frame limit (16 MiB) after base64.
const MAX_SHOT_BYTES: usize = 8 * 1024 * 1024;
/// Live view bit rate on Android (bits/s); `screenrecord`'s default 20 Mb/s
/// is too much for a WebSocket relay. Tune here.
const VIDEO_BITRATE: u32 = 4_000_000;
/// Console ports the Android emulator may use (even numbers).
const EMULATOR_PORTS: std::ops::RangeInclusive<u16> = 5554..=5682;
/// Why a host with devices off can't verify.
const DISABLED: &str = "devices are off on this host (`--no-devices`, the VPS profile): \
                        simulators run on your own Mac or Linux machine; run `sun serve --with-host` \
                        or `sun host run` there";

/// Where the device tools are and how many devices may run at once.
#[derive(Debug, Clone)]
pub struct DeviceConfig {
    /// Boot simulators and emulators here at all (`devices.enabled`). Off on
    /// a VPS kernel box: every tool reports not ready with the fix.
    pub enabled: bool,
    /// Facebook's `idb`, for taps and typing on the iOS Simulator.
    pub idb: PathBuf,
    /// `xcrun` (Xcode).
    pub xcrun: PathBuf,
    /// The Android SDK `emulator`.
    pub emulator: PathBuf,
    /// The Android SDK `adb`.
    pub adb: PathBuf,
    /// The Maestro CLI.
    pub maestro: PathBuf,
    /// Devices running at once on this host; further verifies wait.
    pub max_running: usize,
    /// The KVM device the Android emulator needs (`/dev/kvm` on Linux,
    /// `None` = not checked).
    pub kvm: Option<PathBuf>,
}

impl Default for DeviceConfig {
    /// Tools from `PATH`; `emulator` and `adb` from `$ANDROID_HOME` (or
    /// `$ANDROID_SDK_ROOT`) when they are there. One device at a time.
    fn default() -> Self {
        let sdk = std::env::var_os("ANDROID_HOME")
            .or_else(|| std::env::var_os("ANDROID_SDK_ROOT"))
            .map(PathBuf::from);
        let android = |dir: &str, bin: &str| {
            sdk.as_ref()
                .map(|sdk| sdk.join(dir).join(bin))
                .filter(|path| path.is_file())
                .unwrap_or_else(|| bin.into())
        };
        Self {
            enabled: true,
            idb: "idb".into(),
            xcrun: "xcrun".into(),
            emulator: android("emulator", "emulator"),
            adb: android("platform-tools", "adb"),
            maestro: "maestro".into(),
            max_running: 1,
            kvm: cfg!(target_os = "linux").then(|| "/dev/kvm".into()),
        }
    }
}

/// The host's device runner: config, the running-device cap, the
/// emulator ports in use, the devices up right now (the live view) and the
/// ones agents booted, held per task until its session ends.
pub(crate) struct Devices {
    config: DeviceConfig,
    slots: Semaphore,
    ports: Mutex<HashSet<u16>>,
    live: Mutex<HashMap<String, Live>>,
    held: Mutex<HashMap<String, Device>>,
    idb_ready: OnceCell<bool>,
    /// Live view video streams by the kernel's stream id.
    streams: Mutex<HashMap<String, Arc<video::Feed>>>,
}

/// A device that is up, by the task whose verify booted it. Worktrees are
/// `<data_dir>/work/<task_id>`, so the task is the `cwd`'s last component.
#[derive(Clone)]
struct Live {
    label: String,
    platform: DevicePlatform,
    id: String,
}

/// The task a verify's `cwd` belongs to.
pub(crate) fn task_of(cwd: &Path) -> String {
    cwd.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// A simulator to clone: its display label and what `simctl create` needs.
#[derive(Debug, Clone, PartialEq)]
struct Simulator {
    label: String,
    type_id: String,
    runtime: String,
}

/// What a verify boots from.
enum Target {
    Ios(Simulator),
    Android(String),
}

/// A booted device instance of one verify.
enum Device {
    Ios {
        udid: String,
    },
    Android {
        serial: String,
        port: u16,
        emulator: Child,
    },
}

impl Device {
    /// The id Maestro's `--device` takes.
    fn id(&self) -> &str {
        match self {
            Self::Ios { udid } => udid,
            Self::Android { serial, .. } => serial,
        }
    }

    fn platform(&self) -> DevicePlatform {
        match self {
            Self::Ios { .. } => DevicePlatform::Ios,
            Self::Android { .. } => DevicePlatform::Android,
        }
    }
}

impl Devices {
    pub(crate) fn new(config: DeviceConfig) -> Self {
        Self {
            slots: Semaphore::new(config.max_running.max(1)),
            ports: Mutex::default(),
            live: Mutex::default(),
            held: Mutex::default(),
            idb_ready: OnceCell::new(),
            streams: Mutex::default(),
            config,
        }
    }

    /// What this host can verify on, for `hello`. With devices
    /// off, nothing is ready and each tool says why.
    pub(crate) async fn detect(&self) -> DeviceCapabilities {
        if !self.config.enabled {
            let off = || tool(Err(DISABLED.into()));
            return DeviceCapabilities {
                ios_simulator: off(),
                android_emulator: off(),
                maestro: off(),
            };
        }
        let (ios, android, maestro) = tokio::join!(self.ios(), self.android(), self.maestro());
        DeviceCapabilities {
            ios_simulator: tool(
                ios.map(|sims| (sims.into_iter().map(|s| s.label).collect(), None)),
            ),
            android_emulator: tool(android.map(|avds| (avds, None))),
            maestro: tool(maestro.map(|version| (Vec::new(), Some(version)))),
        }
    }

    /// Runs one verify. `request.cwd` is already checked. `scratch` holds the
    /// flow's working dir (`<scratch>/device-<session>`), removed afterwards.
    pub(crate) async fn verify(
        &self,
        scratch: &Path,
        request: &DeviceRequest,
    ) -> Result<DeviceResult> {
        self.enabled()?;
        // The verify gets a fresh instance: the agent's is done with.
        self.release(&task_of(&request.cwd)).await;
        let deadline = Instant::now() + Duration::from_secs(request.timeout_s);
        let _slot = tokio::time::timeout_at(deadline.into(), self.slots.acquire())
            .await
            .map_err(|_| Error::Unavailable("timed out waiting for a free device slot".into()))?
            .map_err(|_| Error::Unavailable("device runner closed".into()))?;
        let (target, label) = self
            .target(request.platform, request.device.as_deref())
            .await?;
        let failed = |output: String| DeviceResult {
            device: label.clone(),
            passed: false,
            output,
            screenshots: Vec::new(),
        };

        let mut log = String::new();
        if let Some(build) = &request.build {
            let mut cmd = Command::new("sh");
            cmd.arg("-c").arg(build).current_dir(&request.cwd);
            let out = self.run(cmd, request, deadline).await?;
            log.push_str(&out.output);
            if out.code != Some(0) {
                return Ok(failed(format!("build failed\n{log}")));
            }
        }
        let app = resolve(&request.cwd, &request.app)?;
        let flow = resolve(&request.cwd, &request.flow)?;
        let dir = scratch.join(format!("device-{}", request.session_id));
        std::fs::create_dir_all(&dir)
            .map_err(|e| Error::Unavailable(format!("{}: {e}", dir.display())))?;

        let result = match self.boot(request, target, deadline).await {
            Ok(device) => {
                let task = task_of(&request.cwd);
                let live = Live {
                    label: label.clone(),
                    platform: request.platform,
                    id: device.id().to_owned(),
                };
                self.lives().insert(task.clone(), live);
                let passed = self
                    .drive(&device, &app, &flow, &dir, request, deadline, &mut log)
                    .await;
                self.lives().remove(&task);
                self.teardown(device).await;
                passed.map(|passed| DeviceResult {
                    device: label,
                    passed,
                    output: log,
                    screenshots: screenshots(&dir),
                })
            }
            Err(e) => Err(e),
        };
        let _ = std::fs::remove_dir_all(&dir);
        result
    }

    fn enabled(&self) -> Result<()> {
        match self.config.enabled {
            true => Ok(()),
            false => Err(Error::Unavailable(DISABLED.into())),
        }
    }

    /// Maestro is there, and the device to boot from with its label.
    async fn target(
        &self,
        platform: DevicePlatform,
        wanted: Option<&str>,
    ) -> Result<(Target, String)> {
        self.maestro().await.map_err(Error::Unavailable)?;
        Ok(match platform {
            DevicePlatform::Ios => {
                let sims = self.ios().await.map_err(Error::Unavailable)?;
                let sim = pick(&sims, wanted, |s| &s.label)?.clone();
                let label = sim.label.clone();
                (Target::Ios(sim), label)
            }
            DevicePlatform::Android => {
                let avds = self.android().await.map_err(Error::Unavailable)?;
                let avd = pick(&avds, wanted, |a| a)?.clone();
                (Target::Android(avd.clone()), avd)
            }
        })
    }

    /// Runs an agent's Maestro flow on its task's device: the one
    /// up for the task, or a new one booted from `request.platform` and held
    /// for the task (one device slot) until [`Self::release`].
    pub(crate) async fn flow(
        &self,
        scratch: &Path,
        request: &DeviceFlowRequest,
    ) -> Result<DeviceResult> {
        self.enabled()?;
        let task = task_of(&request.cwd);
        let deadline = Instant::now() + Duration::from_secs(request.timeout_s);
        let live = match (self.live_of(&task), request.platform) {
            (Ok(live), _) => live,
            (Err(e), None) => return Err(e),
            (Err(_), Some(platform)) => self.hold(&task, request, platform, deadline).await?,
        };
        // Runs the way a verify does: env and output handling of `run`.
        let call = DeviceRequest {
            session_id: SessionId::generate(),
            cwd: request.cwd.clone(),
            platform: live.platform,
            device: None,
            build: None,
            app: PathBuf::new(),
            flow: PathBuf::new(),
            env: Default::default(),
            timeout_s: request.timeout_s,
        };
        let mut log = String::new();
        if let Some(app) = &request.app {
            let app = resolve(&request.cwd, app)?;
            let mut install = self.install(live.platform, &live.id);
            install.arg(app);
            let out = self.run(install, &call, deadline).await?;
            log.push_str(&out.output);
            if out.code != Some(0) {
                log.push_str("\ninstall failed");
                return Ok(DeviceResult {
                    device: live.label,
                    passed: false,
                    output: log,
                    screenshots: Vec::new(),
                });
            }
        }
        let dir = scratch.join(format!("device-flow-{}", call.session_id));
        let written = std::fs::create_dir_all(&dir)
            .and_then(|()| std::fs::write(dir.join("flow.yaml"), &request.flow));
        if let Err(e) = written {
            let _ = std::fs::remove_dir_all(&dir);
            return Err(Error::Unavailable(format!("{}: {e}", dir.display())));
        }
        let mut maestro = Command::new(&self.config.maestro);
        maestro
            .args(["--device", &live.id, "test", "flow.yaml"])
            .current_dir(&dir);
        let out = self.run(maestro, &call, deadline).await;
        let _ = std::fs::remove_dir_all(&dir);
        let out = out?;
        log.push_str(&out.output);
        Ok(DeviceResult {
            device: live.label,
            passed: out.code == Some(0),
            output: log,
            screenshots: Vec::new(),
        })
    }

    /// Boots a device for `task`'s agent and holds it, with its slot.
    async fn hold(
        &self,
        task: &str,
        request: &DeviceFlowRequest,
        platform: DevicePlatform,
        deadline: Instant,
    ) -> Result<Live> {
        tokio::time::timeout_at(deadline.into(), self.slots.acquire())
            .await
            .map_err(|_| Error::Unavailable("timed out waiting for a free device slot".into()))?
            .map_err(|_| Error::Unavailable("device runner closed".into()))?
            .forget(); // given back by `release`
        let booted = async {
            let (target, label) = self.target(platform, request.device.as_deref()).await?;
            let boot = DeviceRequest {
                session_id: SessionId::generate(),
                cwd: request.cwd.clone(),
                platform,
                device: None,
                build: None,
                app: PathBuf::new(),
                flow: PathBuf::new(),
                env: Default::default(),
                timeout_s: request.timeout_s,
            };
            Ok::<_, Error>((self.boot(&boot, target, deadline).await?, label))
        };
        let (device, label) = match booted.await {
            Ok(booted) => booted,
            Err(e) => {
                self.slots.add_permits(1);
                return Err(e);
            }
        };
        let live = Live {
            label,
            platform,
            id: device.id().to_owned(),
        };
        self.lives().insert(task.to_owned(), live.clone());
        self.held().insert(task.to_owned(), device);
        Ok(live)
    }

    /// Shuts down the device `task`'s agent held, if any, and frees its slot.
    pub(crate) async fn release(&self, task: &str) {
        let Some(device) = self.held().remove(task) else {
            return;
        };
        self.lives().remove(task);
        self.teardown(device).await;
        self.slots.add_permits(1);
    }

    fn held(&self) -> std::sync::MutexGuard<'_, HashMap<String, Device>> {
        self.held.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// `simctl install <udid>` / `adb install -r`; the app path comes next.
    fn install(&self, platform: DevicePlatform, id: &str) -> Command {
        match platform {
            DevicePlatform::Ios => self.xcrun(["simctl", "install", id]),
            DevicePlatform::Android => self.adb(id, ["install", "-r"]),
        }
    }

    /// `idb` answers: the iOS Simulator takes input (checked once).
    async fn idb_ready(&self) -> bool {
        *self
            .idb_ready
            .get_or_init(|| async { probe(&self.config.idb, &["--help"]).await.is_ok() })
            .await
    }

    fn lives(&self) -> std::sync::MutexGuard<'_, HashMap<String, Live>> {
        self.live.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn live_of(&self, task: &str) -> Result<Live> {
        self.lives()
            .get(task)
            .cloned()
            .ok_or_else(|| Error::NotFound(format!("no device is running for task {task}")))
    }

    /// One picture of the screen of `task`'s device: JPEG from the iOS
    /// Simulator, PNG from `adb screencap`.
    /// The live view's fallback when [`Self::video`] can't stream.
    pub(crate) async fn screen(&self, task: &str) -> Result<DeviceScreen> {
        let live = self.live_of(task)?;
        let (program, args, mime) = match live.platform {
            DevicePlatform::Ios => (
                &self.config.xcrun,
                vec!["simctl", "io", &live.id, "screenshot", "--type=jpeg", "-"],
                "image/jpeg",
            ),
            DevicePlatform::Android => (
                &self.config.adb,
                vec!["-s", &live.id, "exec-out", "screencap", "-p"],
                "image/png",
            ),
        };
        let image = probe_bytes(program, &args)
            .await
            .map_err(Error::Unavailable)?;
        Ok(DeviceScreen {
            device: live.label,
            platform: live.platform,
            input: live.platform == DevicePlatform::Android || self.idb_ready().await,
            mime: mime.into(),
            image,
        })
    }

    /// The next H.264 frames of `task`'s device for viewer `stream`, waiting
    /// up to a second for one. The first pull starts the encoder: `adb
    /// exec-out screenrecord --output-format=h264 -` on Android (built in,
    /// no scrcpy-server to push), `idb video-stream --format h264` on the
    /// iOS Simulator (`simctl io recordVideo` only writes files).
    pub(crate) async fn video(&self, task: &str, stream: &str) -> Result<DeviceVideo> {
        if stream.is_empty() || stream.len() > 64 {
            return Err(Error::InvalidInput("stream must be 1–64 characters".into()));
        }
        let live = self.live_of(task)?;
        let input = live.platform == DevicePlatform::Android || self.idb_ready().await;
        let known = {
            let mut streams = self.streams();
            streams.retain(|_, feed| !feed.gone());
            streams.get(stream).filter(|f| f.task == task).cloned()
        };
        let feed = match known {
            Some(feed) => feed,
            None => {
                let encoder = match live.platform {
                    DevicePlatform::Android => {
                        let mut adb = Command::new(&self.config.adb);
                        adb.args(["-s", &live.id, "exec-out", "screenrecord"])
                            .arg("--output-format=h264")
                            .arg(format!("--bit-rate={VIDEO_BITRATE}"))
                            .arg("-");
                        adb
                    }
                    DevicePlatform::Ios if input => {
                        let mut idb = Command::new(&self.config.idb);
                        idb.args(["video-stream", "--format", "h264", "--udid", &live.id]);
                        idb
                    }
                    DevicePlatform::Ios => {
                        return Err(Error::Unavailable(
                            "the iOS Simulator streams video through idb".into(),
                        ));
                    }
                };
                let feed = video::Feed::start(task, encoder).map_err(|e| {
                    Error::Unavailable(format!("cannot start the device's video: {e}"))
                })?;
                self.streams().insert(stream.to_owned(), Arc::clone(&feed));
                feed
            }
        };
        let frames = match feed.pull(Duration::from_secs(1)).await {
            Ok(frames) => frames,
            Err(reason) => {
                self.streams().remove(stream);
                return Err(Error::Unavailable(reason));
            }
        };
        Ok(DeviceVideo {
            device: live.label,
            platform: live.platform,
            input,
            frames: frames.into_iter().map(|data| VideoFrame { data }).collect(),
        })
    }

    fn streams(&self) -> std::sync::MutexGuard<'_, HashMap<String, Arc<video::Feed>>> {
        self.streams.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Sends a tap, text or key to `task`'s device: `adb shell input` on
    /// Android, `idb ui` on the iOS Simulator.
    pub(crate) async fn input(&self, task: &str, input: &DeviceInput) -> Result<()> {
        let live = self.live_of(task)?;
        if let DeviceInput::Text { text } = input
            && (text.is_empty()
                || text.len() > 500
                || !text.bytes().all(|b| (0x20..0x7f).contains(&b)))
        {
            return Err(Error::InvalidInput(
                "text must be 1–500 printable ASCII characters".into(),
            ));
        }
        if live.platform == DevicePlatform::Ios {
            return self.idb_input(&live.id, input).await;
        }
        let command = match input {
            DeviceInput::Tap { x, y } => format!("input tap {x} {y}"),
            DeviceInput::Text { text } => {
                // `input text` reads `%s` as a space; the device shell gets one quoted word.
                let text = text.replace(' ', "%s").replace('\'', r"'\''");
                format!("input text '{text}'")
            }
            DeviceInput::Key { key } => {
                let code = match key {
                    DeviceKey::Home => 3,
                    DeviceKey::Back => 4,
                    DeviceKey::Enter => 66,
                    DeviceKey::Delete => 67,
                };
                format!("input keyevent {code}")
            }
        };
        probe(&self.config.adb, &["-s", &live.id, "shell", &command])
            .await
            .map(drop)
            .map_err(Error::Unavailable)
    }

    /// iOS Simulator input through `idb ui`. Taps arrive in screenshot
    /// pixels; idb wants points, so they are divided by the screen density.
    // ponytail: one `idb describe` per tap; cache the density per device if
    // taps ever need to be faster than ~10/s.
    async fn idb_input(&self, udid: &str, input: &DeviceInput) -> Result<()> {
        if !self.idb_ready().await {
            return Err(Error::InvalidInput(
                "the iOS Simulator takes input through idb: `brew install facebook/fb/idb-companion` \
                 and `pip install fb-idb`"
                    .into(),
            ));
        }
        let args: Vec<String> = match input {
            DeviceInput::Tap { x, y } => {
                let describe = probe(&self.config.idb, &["describe", "--json", "--udid", udid])
                    .await
                    .map_err(Error::Unavailable)?;
                let density = serde_json::from_str::<Value>(&describe)
                    .ok()
                    .and_then(|v| v["screen_dimensions"]["density"].as_f64())
                    .filter(|d| *d > 0.0)
                    .unwrap_or(1.0);
                let point =
                    |pixels: u32| ((f64::from(pixels) / density).round() as u32).to_string();
                vec!["tap".into(), point(*x), point(*y)]
            }
            DeviceInput::Text { text } => vec!["text".into(), text.clone()],
            DeviceInput::Key { key } => match key {
                DeviceKey::Home => vec!["button".into(), "HOME".into()],
                // HID usage ids: Return, Backspace.
                DeviceKey::Enter => vec!["key".into(), "40".into()],
                DeviceKey::Delete => vec!["key".into(), "42".into()],
                DeviceKey::Back => {
                    return Err(Error::InvalidInput("iOS has no back key".into()));
                }
            },
        };
        let mut all = vec!["ui"];
        all.extend(args.iter().map(String::as_str));
        all.extend(["--udid", udid]);
        probe(&self.config.idb, &all)
            .await
            .map(drop)
            .map_err(Error::Unavailable)
    }

    /// Boots `sun-<session>` from `target`, waiting until it is usable.
    async fn boot(
        &self,
        request: &DeviceRequest,
        target: Target,
        deadline: Instant,
    ) -> Result<Device> {
        let device = match target {
            Target::Ios(sim) => {
                let name = format!("sun-{}", request.session_id);
                let mut create = Command::new(&self.config.xcrun);
                create.args(["simctl", "create", &name, &sim.type_id, &sim.runtime]);
                let out = self
                    .must(create, request, deadline, "simctl create")
                    .await?;
                let udid = out
                    .output
                    .lines()
                    .next()
                    .unwrap_or_default()
                    .trim()
                    .to_owned();
                Device::Ios { udid }
            }
            Target::Android(avd) => {
                let port = self.reserve_port().await?;
                let spawned = Command::new(&self.config.emulator)
                    .args(["-avd", &avd, "-port", &port.to_string()])
                    .args(["-read-only", "-no-window", "-no-audio", "-no-boot-anim"])
                    .stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .process_group(0)
                    .kill_on_drop(true)
                    .spawn();
                match spawned {
                    Ok(emulator) => Device::Android {
                        serial: format!("emulator-{port}"),
                        port,
                        emulator,
                    },
                    Err(e) => {
                        self.release_port(port);
                        return Err(Error::Unavailable(format!(
                            "cannot start the emulator: {e}"
                        )));
                    }
                }
            }
        };
        let wait = match &device {
            Device::Ios { udid } => self.xcrun(["simctl", "bootstatus", udid, "-b"]),
            Device::Android { serial, .. } => self.adb(
                serial,
                [
                    "wait-for-device",
                    "shell",
                    r#"while [ "$(getprop sys.boot_completed)" != 1 ]; do sleep 1; done"#,
                ],
            ),
        };
        match self.must(wait, request, deadline, "boot").await {
            Ok(_) => Ok(device),
            Err(e) => {
                self.teardown(device).await;
                Err(e)
            }
        }
    }

    /// Install, flow, final screenshot. `Ok(passed)`; output goes to `log`.
    #[allow(clippy::too_many_arguments)]
    async fn drive(
        &self,
        device: &Device,
        app: &Path,
        flow: &Path,
        dir: &Path,
        request: &DeviceRequest,
        deadline: Instant,
        log: &mut String,
    ) -> Result<bool> {
        let mut install = self.install(device.platform(), device.id());
        install.arg(app);
        let out = self.run(install, request, deadline).await?;
        log.push_str(&out.output);
        if out.code != Some(0) {
            log.push_str("\ninstall failed");
            return Ok(false);
        }

        let mut maestro = Command::new(&self.config.maestro);
        maestro
            .args(["--device", device.id(), "test"])
            .arg(flow)
            .current_dir(dir);
        let out = self.run(maestro, request, deadline).await?;
        log.push_str(&out.output);
        let passed = out.code == Some(0);

        // The final screen, pass or fail; a failed capture only loses the shot.
        let shot = dir.join(FINAL_SHOT);
        let captures = match device {
            Device::Ios { udid } => {
                let mut io = self.xcrun(["simctl", "io", udid, "screenshot"]);
                io.arg(&shot);
                vec![io]
            }
            Device::Android { serial, .. } => {
                let mut pull = self.adb(serial, ["pull", "/sdcard/sun-final.png"]);
                pull.arg(&shot);
                vec![
                    self.adb(
                        serial,
                        ["shell", "screencap", "-p", "/sdcard/sun-final.png"],
                    ),
                    pull,
                ]
            }
        };
        for capture in captures {
            let _ = self
                .run(capture, request, deadline + Duration::from_secs(30))
                .await;
        }
        Ok(passed)
    }

    /// Shuts the instance down and deletes it. Not bound to the verify's
    /// deadline: it must run after a timeout too.
    async fn teardown(&self, device: Device) {
        let calls: Vec<(&Path, Vec<String>)> = match &device {
            Device::Ios { udid } => vec![
                (
                    &self.config.xcrun,
                    vec!["simctl".into(), "shutdown".into(), udid.clone()],
                ),
                (
                    &self.config.xcrun,
                    vec!["simctl".into(), "delete".into(), udid.clone()],
                ),
            ],
            Device::Android { serial, .. } => vec![(
                &self.config.adb,
                vec!["-s".into(), serial.clone(), "emu".into(), "kill".into()],
            )],
        };
        for (program, args) in calls {
            let args: Vec<&str> = args.iter().map(String::as_str).collect();
            if let Err(e) = probe(program, &args).await {
                eprintln!("sun-host: device teardown: {e}");
            }
        }
        if let Device::Android {
            port, mut emulator, ..
        } = device
        {
            let _ = emulator.kill().await;
            self.release_port(port);
        }
    }

    /// iOS simulators to clone, iPhones of the newest runtime first.
    async fn ios(&self) -> std::result::Result<Vec<Simulator>, String> {
        let json = probe(&self.config.xcrun, &["simctl", "list", "-j", "devices", "available"])
            .await
            .map_err(|e| {
                format!("{e}; the iOS Simulator needs macOS with Xcode: install it, then `sudo xcode-select -s /Applications/Xcode.app`")
            })?;
        let sims = parse_simulators(&json)?;
        if sims.is_empty() {
            return Err(
                "no iOS simulator runtime: add one in Xcode → Settings → Components".into(),
            );
        }
        Ok(sims)
    }

    /// Android virtual devices.
    async fn android(&self) -> std::result::Result<Vec<String>, String> {
        probe(&self.config.adb, &["version"]).await.map_err(|e| {
            format!("{e}; install the Android SDK platform-tools and set ANDROID_HOME")
        })?;
        let list = probe(&self.config.emulator, &["-list-avds"])
            .await
            .map_err(|e| {
                format!(
                    "{e}; install the Android emulator (`sdkmanager emulator`) and set ANDROID_HOME"
                )
            })?;
        let avds = parse_avds(&list);
        if avds.is_empty() {
            return Err("no Android virtual device: create one with `avdmanager create avd` or Android Studio's Device Manager".into());
        }
        if let Some(kvm) = self.config.kvm.as_ref().filter(|kvm| !kvm.exists()) {
            return Err(format!(
                "no {}: enable virtualization and add your user to the `kvm` group",
                kvm.display()
            ));
        }
        Ok(avds)
    }

    /// Maestro's version.
    async fn maestro(&self) -> std::result::Result<String, String> {
        let out = probe(&self.config.maestro, &["--version"])
            .await
            .map_err(|e| {
                format!("{e}; install Maestro: `curl -fsSL https://get.maestro.mobile.dev | bash`")
            })?;
        Ok(out
            .lines()
            .rfind(|l| !l.trim().is_empty())
            .unwrap_or_default()
            .trim()
            .to_owned())
    }

    /// A free emulator console port, held until [`Self::release_port`].
    async fn reserve_port(&self) -> Result<u16> {
        let running = probe(&self.config.adb, &["devices"])
            .await
            .unwrap_or_default();
        let mut ports = self.ports.lock().unwrap_or_else(PoisonError::into_inner);
        let port = EMULATOR_PORTS
            .step_by(2)
            .find(|p| !ports.contains(p) && !running.contains(&format!("emulator-{p}")))
            .ok_or_else(|| Error::Unavailable("no free emulator port".into()))?;
        ports.insert(port);
        Ok(port)
    }

    fn release_port(&self, port: u16) {
        self.ports
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&port);
    }

    fn xcrun<const N: usize>(&self, args: [&str; N]) -> Command {
        let mut cmd = Command::new(&self.config.xcrun);
        cmd.args(args);
        cmd
    }

    fn adb<const N: usize>(&self, serial: &str, args: [&str; N]) -> Command {
        let mut cmd = Command::new(&self.config.adb);
        cmd.args(["-s", serial]).args(args);
        cmd
    }

    /// Runs `cmd` with the request's env until `deadline` (redacted output
    /// tail, own process group: [`process::run`]).
    async fn run(
        &self,
        mut cmd: Command,
        request: &DeviceRequest,
        deadline: Instant,
    ) -> Result<ExecOutput> {
        cmd.envs(&request.env);
        let left = deadline
            .saturating_duration_since(Instant::now())
            .as_secs()
            .max(1);
        process::run(
            cmd,
            &ExecRequest {
                cwd: request.cwd.clone(),
                command: String::new(),
                env: request.env.clone(),
                timeout_s: left,
            },
        )
        .await
    }

    /// [`Self::run`] where a non-zero exit is `Unavailable` (the device side broke).
    async fn must(
        &self,
        cmd: Command,
        request: &DeviceRequest,
        deadline: Instant,
        what: &str,
    ) -> Result<ExecOutput> {
        let out = self.run(cmd, request, deadline).await?;
        match out.code {
            Some(0) => Ok(out),
            _ => Err(Error::Unavailable(format!(
                "{what} failed: {}",
                out.output.trim()
            ))),
        }
    }
}

/// A [`DeviceTool`] from a probe: `Ok((devices, version))` or the fix.
fn tool(found: std::result::Result<(Vec<String>, Option<String>), String>) -> DeviceTool {
    match found {
        Ok((devices, version)) => DeviceTool {
            ready: true,
            version,
            devices,
            detail: None,
        },
        Err(why) => DeviceTool {
            detail: Some(why),
            ..DeviceTool::default()
        },
    }
}

/// The `wanted` item (exact label, or the first label starting with it), or
/// the first one.
fn pick<'a, T>(items: &'a [T], wanted: Option<&str>, label: impl Fn(&T) -> &str) -> Result<&'a T> {
    let found = match wanted {
        None => items.first(),
        Some(w) => items
            .iter()
            .find(|i| label(i) == w)
            .or_else(|| items.iter().find(|i| label(i).starts_with(w))),
    };
    found.ok_or_else(|| {
        let all: Vec<&str> = items.iter().map(&label).collect();
        Error::InvalidInput(format!(
            "no device `{}` on this host; available: {}",
            wanted.unwrap_or_default(),
            all.join(", ")
        ))
    })
}

/// `rel` inside `cwd`: relative, no `..`, and no symlink out of it.
fn resolve(cwd: &Path, rel: &Path) -> Result<PathBuf> {
    let bad = || {
        Error::InvalidInput(format!(
            "{} must be a path inside the worktree",
            rel.display()
        ))
    };
    if rel.as_os_str().is_empty()
        || !rel
            .components()
            .all(|c| matches!(c, std::path::Component::Normal(_)))
    {
        return Err(bad());
    }
    let path = cwd
        .join(rel)
        .canonicalize()
        .map_err(|e| Error::InvalidInput(format!("{}: {e}", rel.display())))?;
    path.starts_with(cwd).then_some(path).ok_or_else(bad)
}

/// `simctl list -j devices available` → iOS simulators (not ours), newest
/// runtime first, iPhones first.
fn parse_simulators(json: &str) -> std::result::Result<Vec<Simulator>, String> {
    let value: Value = serde_json::from_str(json).map_err(|e| format!("simctl list: {e}"))?;
    let Some(runtimes) = value["devices"].as_object() else {
        return Err("simctl list: no `devices`".into());
    };
    let mut sims = Vec::new();
    for (runtime, devices) in runtimes.iter().rev() {
        let Some((_, version)) = runtime.rsplit_once("SimRuntime.iOS-") else {
            continue;
        };
        let version = version.replace('-', ".");
        for device in devices.as_array().into_iter().flatten() {
            let (Some(name), Some(type_id)) = (
                device["name"].as_str(),
                device["deviceTypeIdentifier"].as_str(),
            ) else {
                continue;
            };
            if !name.starts_with("sun-") {
                sims.push(Simulator {
                    label: format!("{name} (iOS {version})"),
                    type_id: type_id.into(),
                    runtime: runtime.clone(),
                });
            }
        }
    }
    sims.sort_by_key(|s| !s.label.starts_with("iPhone"));
    Ok(sims)
}

/// `emulator -list-avds` → AVD names (skipping `INFO | …` log lines).
fn parse_avds(text: &str) -> Vec<String> {
    text.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.contains(char::is_whitespace) && !l.contains('|'))
        .map(String::from)
        .collect()
}

/// stdout of a quick tool call, or why it failed.
async fn probe(program: &Path, args: &[&str]) -> std::result::Result<String, String> {
    probe_bytes(program, args)
        .await
        .map(|out| String::from_utf8_lossy(&out).into_owned())
}

/// [`probe`] for binary output (screenshots).
async fn probe_bytes(program: &Path, args: &[&str]) -> std::result::Result<Vec<u8>, String> {
    let call = || format!("`{} {}`", program.display(), args.join(" "));
    let output = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .output();
    match tokio::time::timeout(PROBE_TIMEOUT, output).await {
        Ok(Ok(out)) if out.status.success() => Ok(out.stdout),
        Ok(Ok(out)) => Err(format!(
            "{} failed: {}",
            call(),
            String::from_utf8_lossy(&out.stderr).trim()
        )),
        Ok(Err(e)) => Err(format!("{} cannot run: {e}", call())),
        Err(_) => Err(format!("{} timed out", call())),
    }
}

/// Every PNG under `dir` (Maestro's `takeScreenshot`), by name, then the
/// final shot; capped at [`MAX_SHOTS`] / [`MAX_SHOT_BYTES`] keeping the last.
// ponytail: blocking reads of a few MB on the async thread; move to
// spawn_blocking if verifies share a busy runtime.
fn screenshots(dir: &Path) -> Vec<Screenshot> {
    fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
        for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
            let path = entry.path();
            if path.is_dir() {
                walk(&path, out);
            } else if path
                .extension()
                .is_some_and(|e| e.eq_ignore_ascii_case("png"))
            {
                out.push(path);
            }
        }
    }
    let mut files = Vec::new();
    walk(dir, &mut files);
    files.sort_by_key(|p| (p.file_name().is_some_and(|n| n == FINAL_SHOT), p.clone()));
    let (mut shots, mut bytes) = (Vec::new(), 0);
    for path in files.into_iter().rev() {
        let Ok(png) = std::fs::read(&path) else {
            continue;
        };
        if shots.len() == MAX_SHOTS || bytes + png.len() > MAX_SHOT_BYTES {
            break;
        }
        bytes += png.len();
        shots.push(Screenshot {
            name: path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default(),
            png,
        });
    }
    shots.reverse();
    shots
}

#[cfg(test)]
mod tests;
