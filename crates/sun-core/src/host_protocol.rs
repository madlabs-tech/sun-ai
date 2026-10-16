//! Wire frames of the kernel ↔ runtime host WebSocket.
//! Serde types only; these frames are the protocol.
//!
//! Every WebSocket text message is one JSON frame, internally tagged on
//! `type`. [`HostFrame`] flows host → kernel, [`KernelFrame`] kernel → host.
//! Kernel requests carry an `id`; the host answers each with exactly one
//! [`HostFrame::Reply`] of the same `id`.

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::{
    EffortCatalog, Error, HostEvent, HostId, Readiness, SessionHandle, SessionId, SessionSpec,
    StopMode, TaskId,
};

/// Protocol version sent in [`HostFrame::Hello`]. The kernel rejects a major it
/// does not speak.
pub const HOST_PROTOCOL_VERSION: &str = "2.1";

/// File in the repo root that declares the design-canvas preview: the
/// first line is the dev server command (run by the host), each later line a
/// screen as `<path> [name]` (read by the kernel). Blank and `#` lines are
/// skipped.
pub const PREVIEW_FILE: &str = ".sun-preview";

/// A frame the host sends to the kernel.
// `Hello` is big (capabilities) but sent once per connection.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum HostFrame {
    /// First frame of an enrolment connection: trade a single-use token for a
    /// host key. The kernel answers [`KernelFrame::Enrolled`] and closes.
    Enroll {
        /// The single-use enrolment token from `sun host join <token>`.
        token: String,
        /// Display name for the host (defaults to the machine's hostname).
        name: String,
    },
    /// First frame of a status connection (`sun host join` waiting for its
    /// service): the kernel answers [`KernelFrame::Status`] and closes.
    Status {
        /// The host's id from enrolment.
        host_id: HostId,
        /// The host's secret key from enrolment.
        host_key: String,
    },
    /// First frame of a leave connection (`sun host leave`): detach this host
    /// from every workspace. The kernel answers [`KernelFrame::Left`] and
    /// closes.
    Leave {
        /// The host's id from enrolment.
        host_id: HostId,
        /// The host's secret key from enrolment.
        host_key: String,
    },
    /// First frame of every session connection. Authenticates the host and
    /// tells the kernel which session events the host still holds.
    Hello {
        /// The host's id from enrolment.
        host_id: HostId,
        /// The host's secret key from enrolment.
        host_key: String,
        /// [`HOST_PROTOCOL_VERSION`] of the host.
        protocol: String,
        /// What this host can run.
        capabilities: Capabilities,
        /// Every session the host still buffers, with the highest `seq` the
        /// kernel acked. The host resends every event above it next.
        last_ack: BTreeMap<SessionId, u64>,
    },
    /// One agent event, numbered per session (1, 2, ... gapless). May arrive
    /// more than once after a reconnect; the kernel dedupes on `(session_id, seq)`.
    Event(HostEvent),
    /// Resource use of a live session, sent every 5 s. Not buffered.
    Stats(SessionStats),
    /// Disk use of every task folder on the host, sent every minute.
    /// Not buffered.
    Disk(DiskUse),
    /// A re-probe found the host's capabilities changed since its last
    /// report (protocol 2.1). Sent only on change: every 5 min, on
    /// [`KernelFrame::Reprobe`] and after a session ends on an auth error.
    /// Not buffered: the next `hello` re-probes anyway.
    Capabilities {
        /// Everything the host can run now; replaces the `hello` report.
        capabilities: Capabilities,
    },
    /// The answer to one kernel request.
    Reply {
        /// The request's `id`.
        id: u64,
        /// The result.
        body: ReplyBody,
    },
}

/// A frame the kernel sends to the host.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum KernelFrame {
    /// Answer to [`HostFrame::Enroll`]. The host stores both values.
    #[serde(rename = "enrolled")]
    Enrolled {
        /// The new host's id.
        host_id: HostId,
        /// The host's secret key; sent in every later `hello`.
        host_key: String,
    },
    /// Answer to [`HostFrame::Status`].
    #[serde(rename = "status")]
    Status {
        /// The kernel holds a session connection of this host.
        connected: bool,
        /// The names of the workspaces whose runs may go to it.
        workspaces: Vec<String>,
    },
    /// Answer to [`HostFrame::Leave`]: the host is in no workspace any more.
    #[serde(rename = "left")]
    Left,
    /// Every event of `session_id` up to and including `seq` is stored.
    #[serde(rename = "ack")]
    Ack {
        /// The session.
        session_id: SessionId,
        /// Highest contiguous stored `seq`.
        seq: u64,
    },
    /// Make sure the task's working directory exists. Idempotent: a second call
    /// for the same task returns the same path. Reply: [`ReplyBody::Worktree`].
    #[serde(rename = "worktree.prepare")]
    WorktreePrepare {
        /// Request id.
        id: u64,
        /// What to prepare.
        worktree: Worktree,
    },
    /// Remove the task's working directory (every worktree in it and its
    /// preview checkout) once the task is done or cancelled.
    /// Idempotent. Reply: [`ReplyBody::Done`].
    #[serde(rename = "worktree.remove")]
    WorktreeRemove {
        /// Request id.
        id: u64,
        /// The task.
        task_id: TaskId,
    },
    /// Start a session. `spec.worktree` must be a path returned by
    /// `worktree.prepare`. Reply: [`ReplyBody::Session`].
    #[serde(rename = "session.start")]
    SessionStart {
        /// Request id.
        id: u64,
        /// `RuntimeAdapter::id`, e.g. `claude-code`.
        adapter: String,
        /// The session to start (boxed: the largest frame by far).
        spec: Box<SessionSpec>,
    },
    /// Steer a running session. Reply: [`ReplyBody::Done`].
    #[serde(rename = "session.send")]
    SessionSend {
        /// Request id.
        id: u64,
        /// The session.
        session_id: SessionId,
        /// The message.
        text: String,
    },
    /// Stop a session (idempotent). Reply: [`ReplyBody::Done`].
    #[serde(rename = "session.stop")]
    SessionStop {
        /// Request id.
        id: u64,
        /// The session.
        session_id: SessionId,
        /// Graceful or kill.
        how: StopMode,
    },
    /// Is the session still running? Reply: [`ReplyBody::Alive`].
    #[serde(rename = "probe")]
    Probe {
        /// Request id.
        id: u64,
        /// The session.
        session_id: SessionId,
    },
    /// Run a shell command in a prepared worktree (gates). Reply: [`ReplyBody::Exec`].
    #[serde(rename = "exec")]
    Exec {
        /// Request id.
        id: u64,
        /// What to run.
        request: ExecRequest,
    },
    /// Write context files (`.sun/…`) into a prepared worktree before a
    /// session starts. `.sun/context/` is cleared first. Reply:
    /// [`ReplyBody::Done`].
    #[serde(rename = "worktree.write")]
    WorktreeWrite {
        /// Request id.
        id: u64,
        /// A path returned by `worktree.prepare`.
        cwd: PathBuf,
        /// The files; every path is relative and starts with `.sun/`.
        files: Vec<SessionFile>,
    },
    /// Build, install and check the app on a fresh simulator or emulator.
    /// Reply: [`ReplyBody::Device`].
    #[serde(rename = "device.verify")]
    DeviceVerify {
        /// Request id.
        id: u64,
        /// What to verify.
        request: DeviceRequest,
    },
    /// One picture of the screen of the device a task's verify is running on
    /// (the live view). Reply: [`ReplyBody::Screen`];
    /// `not_found` while no device is up for the task.
    #[serde(rename = "device.screen")]
    DeviceScreen {
        /// Request id.
        id: u64,
        /// The task.
        task_id: TaskId,
    },
    /// The next H.264 frames of that device's screen: the first
    /// call of a `stream` starts its encoder, each call waits up to a second
    /// for frames. Reply: [`ReplyBody::Video`]; `not_found` while no device
    /// is up for the task, `unavailable` when the host can't stream (the
    /// viewer falls back to `device.screen`).
    #[serde(rename = "device.video")]
    DeviceVideo {
        /// Request id.
        id: u64,
        /// The task.
        task_id: TaskId,
        /// The viewer's stream, chosen by the kernel (1–64 characters);
        /// unpulled for 5 s, the host stops it.
        stream: String,
    },
    /// A tap, text or key on that device. Reply: [`ReplyBody::Done`];
    /// `invalid_input` when the device takes no input.
    #[serde(rename = "device.input")]
    DeviceInput {
        /// Request id.
        id: u64,
        /// The task.
        task_id: TaskId,
        /// What to send.
        input: DeviceInput,
    },
    /// Run a Maestro flow on the task's device for its agent,
    /// first booting one held for the task when none is up. Reply:
    /// [`ReplyBody::Device`] without screenshots (`device.screen` shows it).
    #[serde(rename = "device.flow")]
    DeviceFlow {
        /// Request id.
        id: u64,
        /// What to run.
        request: DeviceFlowRequest,
    },
    /// Start (or reuse) the dev server of a task's `before` or `after` checkout
    /// on the host's loopback. Reply: [`ReplyBody::Done`] once it accepts
    /// connections.
    #[serde(rename = "preview.start")]
    PreviewStart {
        /// Request id.
        id: u64,
        /// What to start.
        request: PreviewRequest,
    },
    /// One HTTP request to a running preview, tunnelled over this socket.
    /// Reply: [`ReplyBody::Http`].
    #[serde(rename = "preview.http")]
    PreviewHttp {
        /// Request id.
        id: u64,
        /// The task.
        task_id: TaskId,
        /// Which checkout.
        side: PreviewSide,
        /// The request, path included.
        request: HttpRequest,
    },
    /// A PNG of one page of a task's preview at a viewport size, from a
    /// headless browser on the host (canvas crops, evidence and the visual
    /// gate). Starts the side's dev server when none runs. Reply:
    /// [`ReplyBody::Png`]; `unavailable` when the host has no browser.
    #[serde(rename = "preview.screenshot")]
    PreviewScreenshot {
        /// Request id.
        id: u64,
        /// What to take.
        request: ScreenshotRequest,
    },
    /// Stop both dev servers of a task (idempotent). Reply: [`ReplyBody::Done`].
    #[serde(rename = "preview.stop")]
    PreviewStop {
        /// Request id.
        id: u64,
        /// The task.
        task_id: TaskId,
    },
    /// Probe every runtime again now ("Check again"; protocol 2.1). The host
    /// sends [`HostFrame::Capabilities`] first when anything changed, then
    /// [`ReplyBody::Done`].
    #[serde(rename = "reprobe")]
    Reprobe {
        /// Request id.
        id: u64,
    },
}

string_enum! {
    /// Which checkout a preview serves (before/after).
    pub enum PreviewSide {
        /// The base branch the task was cut from.
        Before => "before",
        /// The task's branch (its worktree, with uncommitted agent edits).
        After => "after",
    }
}

/// A [`KernelFrame::PreviewStart`] request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PreviewRequest {
    /// The task's worktree, prepared like `worktree.prepare`. `before`
    /// needs a `repo` worktree.
    pub worktree: Worktree,
    /// Which checkout.
    pub side: PreviewSide,
    /// URL prefix the kernel serves the preview under, e.g. `/preview/<token>/`.
    /// Given to the command as `SUN_PREVIEW_BASE`; the dev server must serve
    /// its app under it. A different base restarts a running server.
    pub base_path: String,
    /// Give up when the server does not accept connections within this.
    pub timeout_s: u64,
}

/// A [`KernelFrame::PreviewScreenshot`] request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScreenshotRequest {
    /// The task's worktree (to start the dev server if needed).
    pub worktree: Worktree,
    /// Which checkout.
    pub side: PreviewSide,
    /// Path and query under the preview base, starting with `/`.
    pub path: String,
    /// Viewport width in CSS pixels.
    pub width: u32,
    /// Viewport height in CSS pixels.
    pub height: u32,
}

/// An HTTP request tunnelled to a preview. Body is base64 on the wire.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HttpRequest {
    /// `GET`, `POST`, …
    pub method: String,
    /// Path and query, starting with `/`.
    pub path: String,
    /// End-to-end headers as `[name, value]` pairs.
    pub headers: Vec<(String, String)>,
    /// Request body.
    #[serde(with = "base64_bytes")]
    pub body: Vec<u8>,
}

/// A preview's answer to an [`HttpRequest`]. Body is base64 on the wire.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HttpResponse {
    /// Status code.
    pub status: u16,
    /// Headers as `[name, value]` pairs.
    pub headers: Vec<(String, String)>,
    /// Response body.
    #[serde(with = "base64_bytes")]
    pub body: Vec<u8>,
}

/// Where a session works.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Worktree {
    /// A git worktree on `branch`, cut from a cached bare clone of `url`.
    Repo {
        /// The task.
        task_id: TaskId,
        /// Clone URL (https, ssh or a local path).
        url: String,
        /// Branch to start from; `None` = the remote's default branch.
        base: Option<String>,
        /// The branch to work on (a task's `sun/<KEY>-<slug>`).
        branch: String,
    },
    /// An empty task folder, for tasks with no repo.
    Folder {
        /// The task.
        task_id: TaskId,
    },
    /// A multi-repo task: a session dir holding one worktree per repo,
    /// each at `<name>/` on its `branch`, cut from its bare clone.
    Repos {
        /// The task.
        task_id: TaskId,
        /// The repos, in the task's merge order.
        repos: Vec<RepoCheckout>,
    },
}

impl Worktree {
    /// The task the worktree belongs to.
    pub fn task_id(&self) -> TaskId {
        match self {
            Self::Repo { task_id, .. } | Self::Folder { task_id } | Self::Repos { task_id, .. } => {
                *task_id
            }
        }
    }

    /// The branch the worktree is on (a multi-repo task: its first repo's);
    /// `None` for a plain folder.
    pub fn branch(&self) -> Option<&str> {
        match self {
            Self::Repo { branch, .. } => Some(branch),
            Self::Repos { repos, .. } => repos.first().map(|r| r.branch.as_str()),
            Self::Folder { .. } => None,
        }
    }
}

/// One repo of a [`Worktree::Repos`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepoCheckout {
    /// Its folder in the session dir: `[A-Za-z0-9._-]`, not starting with `.` or `-`.
    pub name: String,
    /// Clone URL (https, ssh or a local path).
    pub url: String,
    /// Branch to start from; `None` = the remote's default branch.
    pub base: Option<String>,
    /// The branch to work on (the task's `sun/<KEY>-<slug>`).
    pub branch: String,
}

/// A file for [`KernelFrame::WorktreeWrite`]: one `.sun/` file of a
/// session dir.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionFile {
    /// Path relative to the session dir, under `.sun/`.
    pub path: String,
    /// Content.
    pub contents: String,
    /// `false`: only written if the file does not exist yet (the agent owns it).
    pub overwrite: bool,
}

/// A command for [`KernelFrame::Exec`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecRequest {
    /// Working directory: a path returned by `worktree.prepare`.
    pub cwd: PathBuf,
    /// Shell command, run as `sh -c <command>`.
    pub command: String,
    /// Extra environment. Values are treated as secrets and redacted from output.
    pub env: BTreeMap<String, String>,
    /// Kill the command's process group after this many seconds.
    pub timeout_s: u64,
}

/// Result of an [`ExecRequest`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecOutput {
    /// Exit code; `None` when killed by a signal or the timeout.
    pub code: Option<i32>,
    /// stdout then stderr, redacted, keeping the last 64 KiB.
    pub output: String,
    /// The timeout fired.
    pub timed_out: bool,
}

/// What a host can run, sent in [`HostFrame::Hello`] and
/// [`HostFrame::Capabilities`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Capabilities {
    /// `std::env::consts::OS`, e.g. `macos`, `linux`.
    pub os: String,
    /// `std::env::consts::ARCH`, e.g. `aarch64`.
    pub arch: String,
    /// Logical CPUs.
    pub cpus: u32,
    /// One entry per runtime adapter the host has.
    pub runtimes: Vec<RuntimeCapability>,
    /// Simulators, emulators and Maestro. Absent from older hosts.
    #[serde(default)]
    pub devices: DeviceCapabilities,
    /// Execution environments the host can prepare:
    /// `bare` always, `container` and `vm` when their CLI answers. Absent
    /// from older hosts (= bare only).
    #[serde(default)]
    pub exec_envs: Vec<ExecEnvCapability>,
}

/// One execution environment kind and whether it works on the host.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecEnvCapability {
    /// `bare`, `container` or `vm` (the [`crate::ExecEnvSpec`] tag).
    pub kind: String,
    /// Usable now.
    pub ready: bool,
    /// The engine's version (`docker info`, `limactl --version`), when ready.
    pub version: Option<String>,
    /// When not ready: what is missing and how to fix it.
    pub detail: Option<String>,
}

/// Device tooling found on a host. The kernel registers the
/// `ios-simulator` / `android-emulator` capability when that tool and
/// `maestro` are ready.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceCapabilities {
    /// Xcode's iOS Simulator (`xcrun simctl`), macOS only.
    pub ios_simulator: DeviceTool,
    /// The Android SDK emulator and `adb`.
    pub android_emulator: DeviceTool,
    /// The Maestro CLI, which runs the flows.
    pub maestro: DeviceTool,
}

/// One device tool and whether it works.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceTool {
    /// Usable now.
    pub ready: bool,
    /// Version, when the tool reports one.
    pub version: Option<String>,
    /// What it can boot: simulators (`iPhone 15 (iOS 17.0)`) or AVD names.
    pub devices: Vec<String>,
    /// When not ready: what is missing and how to fix it.
    pub detail: Option<String>,
}

/// Which device family a [`DeviceRequest`] runs on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DevicePlatform {
    /// iOS Simulator (macOS hosts).
    Ios,
    /// Android emulator.
    Android,
}

/// A [`KernelFrame::DeviceVerify`] request. The host boots its own device
/// instance for it and deletes it afterwards.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceRequest {
    /// Names the device instance (`sun-<session_id>`).
    pub session_id: SessionId,
    /// Working directory: a path returned by `worktree.prepare`.
    pub cwd: PathBuf,
    /// iOS or Android.
    pub platform: DevicePlatform,
    /// Simulator or AVD name to start from; `None` = the first available.
    pub device: Option<String>,
    /// Shell command that builds the app in `cwd`; `None` = already built.
    pub build: Option<String>,
    /// The built app relative to `cwd`: a simulator `.app` or an `.apk`.
    pub app: PathBuf,
    /// Maestro flow file or folder relative to `cwd`.
    pub flow: PathBuf,
    /// Extra environment for the build and the flow. Values are secrets.
    pub env: BTreeMap<String, String>,
    /// Seconds for the whole verify: build, boot, install and flow.
    pub timeout_s: u64,
}

/// A [`KernelFrame::DeviceFlow`] request: an agent driving its task's device.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceFlowRequest {
    /// The task's worktree: a path returned by `worktree.prepare`.
    pub cwd: PathBuf,
    /// Platform to boot when no device is up for the task; `None` = the one
    /// that is up (`not_found` when none is).
    pub platform: Option<DevicePlatform>,
    /// Simulator or AVD name to boot from; `None` = the first available.
    pub device: Option<String>,
    /// App to install first, relative to `cwd`; `None` = already installed.
    pub app: Option<PathBuf>,
    /// The Maestro flow, as YAML.
    pub flow: String,
    /// Seconds for boot, install and flow.
    pub timeout_s: u64,
}

/// What a [`DeviceRequest`] produced.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceResult {
    /// The device that ran, e.g. `iPhone 15 (iOS 17.0)`.
    pub device: String,
    /// The build and the flow passed.
    pub passed: bool,
    /// Build, install and Maestro output, redacted (each part's last 64 KiB).
    pub output: String,
    /// Screenshots from the flow (`takeScreenshot`), then one of the final screen.
    pub screenshots: Vec<Screenshot>,
}

/// One PNG from a device. Base64 on the wire.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Screenshot {
    /// File name, e.g. `login.png`.
    pub name: String,
    /// The PNG bytes.
    #[serde(with = "base64_bytes")]
    pub png: Vec<u8>,
}

/// One picture of a live device screen ([`KernelFrame::DeviceScreen`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceScreen {
    /// The device, e.g. `iPhone 15 (iOS 17.0)`.
    pub device: String,
    /// iOS or Android.
    pub platform: DevicePlatform,
    /// The device takes [`DeviceInput`] (Android today; `simctl` has no input).
    pub input: bool,
    /// `image/jpeg` or `image/png`.
    pub mime: String,
    /// The image in device pixels (tap coordinates use the same space).
    #[serde(with = "base64_bytes")]
    pub image: Vec<u8>,
}

/// Frames of a live device's video ([`KernelFrame::DeviceVideo`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceVideo {
    /// The device, as in [`DeviceScreen`].
    pub device: String,
    /// iOS or Android.
    pub platform: DevicePlatform,
    /// The device takes [`DeviceInput`].
    pub input: bool,
    /// H.264 Annex B access units, one picture each, in order; the first
    /// of a stream carries SPS/PPS and an IDR. Empty when the screen didn't
    /// change. Coded size = device pixels (tap coordinates).
    pub frames: Vec<VideoFrame>,
}

/// One H.264 access unit of [`DeviceVideo`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VideoFrame {
    /// Annex B bytes (base64 on the wire).
    #[serde(with = "base64_bytes")]
    pub data: Vec<u8>,
}

/// Input for a live device ([`KernelFrame::DeviceInput`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum DeviceInput {
    /// A tap at a point of the screen image, in device pixels.
    Tap {
        /// From the left.
        x: u32,
        /// From the top.
        y: u32,
    },
    /// Typed into the focused field: printable ASCII, at most 500 characters.
    Text {
        /// What to type.
        text: String,
    },
    /// A hardware or keyboard key.
    Key {
        /// Which.
        key: DeviceKey,
    },
}

string_enum! {
    /// A key a live device accepts.
    pub enum DeviceKey {
        /// Home screen.
        Home => "home",
        /// Back.
        Back => "back",
        /// Return / submit.
        Enter => "enter",
        /// Delete the character before the cursor.
        Delete => "delete",
    }
}

/// Serde for bytes as standard (padded) base64. Hand-rolled: sun-core keeps
/// its dependency list short (`scripts/check-deps.sh`).
/// Standard padded base64, as images travel on the wire.
pub fn base64(bytes: &[u8]) -> String {
    base64_bytes::encode(bytes)
}

mod base64_bytes {
    use serde::{Deserialize, Deserializer, Serializer};

    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

    pub fn serialize<S: Serializer>(bytes: &[u8], s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&encode(bytes))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<u8>, D::Error> {
        decode(&String::deserialize(d)?).ok_or_else(|| serde::de::Error::custom("invalid base64"))
    }

    pub(super) fn encode(bytes: &[u8]) -> String {
        let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
        for chunk in bytes.chunks(3) {
            let n = chunk
                .iter()
                .enumerate()
                .fold(0u32, |n, (i, b)| n | u32::from(*b) << (16 - 8 * i));
            for i in 0..4 {
                match i <= chunk.len() {
                    true => out.push(char::from(ALPHABET[(n >> (18 - 6 * i)) as usize & 63])),
                    false => out.push('='),
                }
            }
        }
        out
    }

    pub(super) fn decode(text: &str) -> Option<Vec<u8>> {
        let text = text.as_bytes();
        if !text.len().is_multiple_of(4) {
            return None;
        }
        let last = text.len() / 4;
        let mut out = Vec::with_capacity(last * 3);
        for (index, chunk) in text.chunks(4).enumerate() {
            let pad = chunk.iter().rev().take_while(|c| **c == b'=').count();
            if pad > 2 || (pad > 0 && index + 1 != last) {
                return None;
            }
            let mut n = 0u32;
            for (i, c) in chunk[..4 - pad].iter().enumerate() {
                n |= value(*c)? << (18 - 6 * i);
            }
            out.extend_from_slice(&n.to_be_bytes()[1..4 - pad]);
        }
        Some(out)
    }

    fn value(c: u8) -> Option<u32> {
        let v = match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            _ => return None,
        };
        Some(u32::from(v))
    }
}

/// One runtime adapter on a host and whether it is ready.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuntimeCapability {
    /// `RuntimeAdapter::id`.
    pub adapter: String,
    /// Its readiness check.
    pub readiness: Readiness,
    /// The effort levels the CLI offers, tool-wide and per model
    /// (`effort_options`, `models`). Absent from older hosts (= none).
    #[serde(flatten)]
    pub effort: EffortCatalog,
}

/// Resource use of a session's process tree.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionStats {
    /// The session.
    pub session_id: SessionId,
    /// Resident memory of the whole process tree, in bytes.
    pub rss_bytes: u64,
    /// CPU use of the whole tree, in percent of one core: each process's
    /// lifetime average (CPU seconds over seconds alive), summed.
    pub cpu_percent: f32,
    /// Seconds since the session started.
    pub uptime_s: u64,
}

/// Bytes on disk per task folder (worktrees and preview checkout).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiskUse {
    /// `(task, bytes)` for every task folder the host still has.
    pub tasks: Vec<(TaskId, u64)>,
}

/// The body of a [`HostFrame::Reply`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ReplyBody {
    /// `worktree.prepare`: the host path of the working directory.
    Worktree {
        /// Absolute path on the host.
        path: PathBuf,
    },
    /// `session.start`: the started session.
    Session {
        /// Its handle.
        handle: SessionHandle,
    },
    /// `session.send`, `session.stop`, `worktree.write`, `device.input`,
    /// `preview.start`, `preview.stop`: done.
    Done,
    /// `probe`.
    Alive {
        /// The session is still running.
        alive: bool,
    },
    /// `exec`.
    Exec {
        /// The command's result.
        output: ExecOutput,
    },
    /// `device.verify`.
    Device {
        /// The verify's result.
        result: DeviceResult,
    },
    /// `device.screen`.
    Screen {
        /// The picture.
        screen: DeviceScreen,
    },
    /// `device.video`.
    Video {
        /// The frames.
        video: DeviceVideo,
    },
    /// `preview.http`.
    Http {
        /// The dev server's answer.
        response: HttpResponse,
    },
    /// `preview.screenshot`.
    Png {
        /// The PNG bytes (base64 on the wire).
        #[serde(with = "base64_bytes")]
        png: Vec<u8>,
    },
    /// Any request failed.
    Error {
        /// `invalid_input`, `not_found`, `unavailable` or `protocol`.
        code: String,
        /// Redacted message.
        message: String,
    },
}

impl From<&Error> for ReplyBody {
    fn from(error: &Error) -> Self {
        let (code, message) = match error {
            // The protocol has no `forbidden` code; hosts never authorize.
            Error::InvalidInput(m) | Error::Forbidden(m) | Error::PlanLimit(m) => {
                ("invalid_input", m.clone())
            }
            Error::Refused { message, .. } => ("invalid_input", message.clone()),
            Error::NotFound(m) => ("not_found", m.clone()),
            Error::Unavailable(m) => ("unavailable", m.clone()),
            Error::Protocol(m) => ("protocol", m.clone()),
            other @ Error::InvalidTransition { .. } => ("protocol", other.to_string()),
        };
        Self::Error {
            code: code.into(),
            message,
        }
    }
}

impl ReplyBody {
    /// Turns an [`Error`](Self::Error) reply back into [`Error`]; other bodies
    /// pass through.
    pub fn into_result(self) -> crate::Result<Self> {
        match self {
            Self::Error { code, message } => Err(match code.as_str() {
                "invalid_input" => Error::InvalidInput(message),
                "not_found" => Error::NotFound(message),
                "unavailable" => Error::Unavailable(message),
                _ => Error::Protocol(message),
            }),
            body => Ok(body),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{EffortLevel, ModelEffort, SunEvent};

    #[test]
    fn reprobe_and_capabilities_frames_carry_the_effort_catalog() {
        let reprobe = KernelFrame::Reprobe { id: 3 };
        let json = serde_json::to_value(&reprobe).unwrap();
        assert_eq!(json, serde_json::json!({"type": "reprobe", "id": 3}));
        assert_eq!(
            serde_json::from_value::<KernelFrame>(json).unwrap(),
            reprobe
        );

        let high = EffortLevel {
            value: "high".into(),
            label: "High".into(),
            description: None,
        };
        let runtime = RuntimeCapability {
            adapter: "codex".into(),
            readiness: Readiness {
                installed: true,
                version: Some("0.160.0".into()),
                logged_in: true,
                detail: None,
            },
            effort: EffortCatalog {
                effort_options: vec![high.clone()],
                models: vec![ModelEffort {
                    id: "gpt-5-codex".into(),
                    label: "GPT-5 Codex".into(),
                    effort_options: vec![high],
                    default_effort: Some("high".into()),
                }],
            },
        };
        let frame = HostFrame::Capabilities {
            capabilities: Capabilities {
                os: "linux".into(),
                arch: "x86_64".into(),
                cpus: 2,
                runtimes: vec![runtime],
                devices: DeviceCapabilities::default(),
                exec_envs: Vec::new(),
            },
        };
        let json = serde_json::to_value(&frame).unwrap();
        assert_eq!(json["type"], "capabilities");
        // Flat on the runtime, next to `readiness`.
        let runtime = &json["capabilities"]["runtimes"][0];
        assert_eq!(
            runtime["effort_options"][0],
            serde_json::json!({"value": "high", "label": "High"})
        );
        assert_eq!(runtime["models"][0]["id"], "gpt-5-codex");
        assert_eq!(runtime["models"][0]["default_effort"], "high");
        assert_eq!(serde_json::from_value::<HostFrame>(json).unwrap(), frame);

        // A 2.0 host reports no effort: it still parses, with none.
        let old: RuntimeCapability = serde_json::from_value(serde_json::json!({
            "adapter": "claude-code",
            "readiness": {"installed": true, "version": "2.1.291", "logged_in": false, "detail": null}
        }))
        .unwrap();
        assert_eq!(old.effort, EffortCatalog::default());
    }

    #[test]
    fn frames_are_tagged_snake_case_and_round_trip() {
        let session_id = SessionId::generate();
        let event = HostFrame::Event(HostEvent {
            session_id,
            seq: 3,
            event: SunEvent::Exited { code: Some(0) },
        });
        let json = serde_json::to_value(&event).unwrap();
        assert_eq!(
            json,
            serde_json::json!({
                "type": "event",
                "session_id": session_id.to_string(),
                "seq": 3,
                "event": {"kind": "exited", "code": 0},
            })
        );
        assert_eq!(serde_json::from_value::<HostFrame>(json).unwrap(), event);

        let stop = KernelFrame::SessionStop {
            id: 7,
            session_id,
            how: StopMode::Kill,
        };
        let json = serde_json::to_value(&stop).unwrap();
        assert_eq!(json["type"], "session.stop");
        assert_eq!(json["how"], "kill");
        assert_eq!(serde_json::from_value::<KernelFrame>(json).unwrap(), stop);
    }

    #[test]
    fn device_frames_carry_screenshots_as_base64() {
        let reply = ReplyBody::Device {
            result: DeviceResult {
                device: "iPhone 15 (iOS 17.0)".into(),
                passed: true,
                output: "ok".into(),
                screenshots: vec![Screenshot {
                    name: "login.png".into(),
                    png: vec![0x89, b'P', b'N', b'G'],
                }],
            },
        };
        let json = serde_json::to_value(&reply).unwrap();
        assert_eq!(json["kind"], "device");
        assert_eq!(json["result"]["screenshots"][0]["png"], "iVBORw==");
        assert_eq!(serde_json::from_value::<ReplyBody>(json).unwrap(), reply);
        let input: DeviceInput = serde_json::from_str(r#"{"kind":"key","key":"enter"}"#).unwrap();
        assert_eq!(
            input,
            DeviceInput::Key {
                key: DeviceKey::Enter
            }
        );
        assert!(serde_json::from_str::<DeviceInput>(r#"{"kind":"tap","x":1}"#).is_err());
        for (bytes, text) in [
            (&b""[..], ""),
            (b"f", "Zg=="),
            (b"fo", "Zm8="),
            (b"foo", "Zm9v"),
            (b"foob", "Zm9vYg=="),
            (&[0xfb, 0xff, 0xbf][..], "+/+/"),
        ] {
            assert_eq!(base64_bytes::encode(bytes), text);
            assert_eq!(base64_bytes::decode(text).unwrap(), bytes);
        }
        for bad in ["Zg=", "Zg==Zm8=", "Z===", "Zm9*"] {
            assert_eq!(base64_bytes::decode(bad), None, "{bad}");
        }

        // Older hosts send no `devices`.
        let caps: Capabilities = serde_json::from_value(
            serde_json::json!({"os":"linux","arch":"x86_64","cpus":2,"runtimes":[]}),
        )
        .unwrap();
        assert!(!caps.devices.maestro.ready);

        let verify = serde_json::json!({"type":"device.verify","id":1,"request":{
            "session_id": SessionId::generate().to_string(), "cwd":"/w", "platform":"android",
            "device":null, "build":"./gradlew assembleDebug", "app":"app.apk", "flow":"flow.yaml",
            "env":{}, "timeout_s":900}});
        let frame: KernelFrame = serde_json::from_value(verify).unwrap();
        assert!(matches!(
            frame,
            KernelFrame::DeviceVerify {
                request: DeviceRequest {
                    platform: DevicePlatform::Android,
                    ..
                },
                ..
            }
        ));
    }

    #[test]
    fn error_replies_round_trip_to_errors() {
        let body = ReplyBody::from(&Error::NotFound("session x".into()));
        assert_eq!(
            serde_json::to_value(&body).unwrap(),
            serde_json::json!({"kind": "error", "code": "not_found", "message": "session x"})
        );
        assert_eq!(body.into_result(), Err(Error::NotFound("session x".into())));
        assert_eq!(ReplyBody::Done.into_result(), Ok(ReplyBody::Done));
    }
}
