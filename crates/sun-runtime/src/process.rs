//! Shared JSONL process driver: runs an agent CLI that prints one JSON value
//! per stdout line, feeds each line to a CLI-specific [`Normalizer`], and
//! turns the result into the session's [`SunEvent`] stream.
//!
//! The driver owns the rules every JSONL adapter shares: redaction, "EOF
//! without a finished turn is an error", stderr limit detection, `Exited` last,
//! graceful/kill stop, and the event log used by `export`. [`replay`] runs the
//! same rules over a recorded fixture, so tests exercise the real path.
//!
//! Also shared by every adapter: the session registry ([`Sessions`]), the
//! readiness probe ([`readiness`]) and small text helpers.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::{Arc, Mutex as SyncMutex};
use std::task::{Context, Poll};
use std::time::Duration;

use futures_core::Stream;
use sun_core::redact::redact;
use sun_core::{
    BoxStream, EXEC_SHIMS_ENV, Error, Readiness, Result, SessionId, StopMode, SunEvent,
};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{ChildStdin, Command};
use tokio::sync::{Mutex, mpsc, watch};
use tokio::time::Instant;

/// How long a graceful stop waits after closing stdin before it kills.
pub const GRACE: Duration = Duration::from_secs(10);

/// How much of stderr is kept for error messages and limit detection.
const STDERR_TAIL: usize = 4096;

/// Per-CLI translation of stdout lines (**Strategy**).
pub trait Normalizer: Send + 'static {
    /// Appends the events for one parsed stdout line to `out`. Returns `true`
    /// when the line ends a turn (Claude `result`, Codex `turn.completed|failed`).
    fn line(&mut self, line: &serde_json::Value, out: &mut Vec<SunEvent>) -> bool;

    /// Lines to write to stdin, drained after every stdout line: replies to
    /// the CLI, e.g. the next ACP handshake request. Most CLIs need none.
    fn replies(&mut self) -> Vec<String> {
        Vec::new()
    }

    /// `false` for a CLI that ignores stdin EOF (OpenCode): once its last turn
    /// ended it is killed right away instead of after [`GRACE`].
    fn exits_on_eof(&self) -> bool {
        true
    }
}

/// Parses, normalizes and redacts one raw stdout line. Lines that aren't JSON
/// (a cut last line after a crash, stray logs) are skipped.
fn feed(
    normalizer: &mut impl Normalizer,
    raw: &str,
    secrets: &[&str],
    out: &mut Vec<SunEvent>,
) -> bool {
    let Ok(value) = serde_json::from_str(raw) else {
        return false;
    };
    let start = out.len();
    let turn_ended = normalizer.line(&value, out);
    for event in &mut out[start..] {
        redact_event(event, secrets);
    }
    turn_ended
}

/// The closing events: an error if the turn never finished (unless a stop was
/// asked for), then `Exited`.
fn finish(
    cli: &str,
    open_turns: usize,
    stopped: bool,
    code: Option<i32>,
    stderr: &str,
    secrets: &[&str],
) -> Vec<SunEvent> {
    let mut out = Vec::new();
    if open_turns > 0 && !stopped {
        let stderr = stderr.trim();
        out.push(
            crate::limit::detect(stderr, chrono::Utc::now()).unwrap_or_else(|| SunEvent::Error {
                message: match code {
                    Some(code) => {
                        format!("{cli} exited with code {code} before finishing the turn")
                    }
                    None => format!("{cli} was killed before finishing the turn"),
                } + &if stderr.is_empty() {
                    String::new()
                } else {
                    format!(": {stderr}")
                },
            }),
        );
    }
    out.push(SunEvent::Exited { code });
    for event in &mut out {
        redact_event(event, secrets);
    }
    out
}

/// Replays recorded stdout through the same rules as a live process, for
/// fixture tests. The recording counts as one turn.
pub fn replay(
    cli: &str,
    mut normalizer: impl Normalizer,
    jsonl: &str,
    exit_code: Option<i32>,
    stderr: &str,
) -> Vec<SunEvent> {
    let mut out = Vec::new();
    let mut open_turns = 1usize;
    for raw in jsonl.lines() {
        if feed(&mut normalizer, raw, &[], &mut out) {
            open_turns = open_turns.saturating_sub(1);
        }
    }
    out.extend(finish(cli, open_turns, false, exit_code, stderr, &[]));
    out
}

/// Passes every text field of `event` through the shared secret filter.
pub(crate) fn redact_event(event: &mut SunEvent, secrets: &[&str]) {
    let text = match event {
        SunEvent::Text { text, .. } => text,
        SunEvent::ToolCall { summary, .. }
        | SunEvent::ToolResult { summary, .. }
        | SunEvent::DoneClaimed { summary } => summary,
        SunEvent::FileChanged { path } => path,
        SunEvent::LimitHit { raw, .. } => raw,
        SunEvent::Error { message } => message,
        SunEvent::Plan { todos } => {
            for todo in todos {
                todo.text = redact(&todo.text, secrets);
            }
            return;
        }
        SunEvent::Usage { .. } | SunEvent::Exited { .. } => return,
    };
    *text = redact(text, secrets);
}

/// Stable 64-bit FNV-1a hash of a tool's arguments, for loop detection.
/// `serde_json` maps are key-sorted, so equal arguments hash equally.
pub fn args_hash(args: &serde_json::Value) -> u64 {
    args.to_string()
        .bytes()
        .fold(0xcbf2_9ce4_8422_2325, |hash, byte| {
            (hash ^ u64::from(byte)).wrapping_mul(0x0000_0100_0000_01b3)
        })
}

/// Cuts `text` to at most `max` chars, marking the cut with `…`.
pub fn truncate(text: &str, max: usize) -> String {
    let text = text.trim();
    match text.char_indices().nth(max) {
        Some((cut, _)) => format!("{}…", &text[..cut]),
        None => text.to_owned(),
    }
}

/// stdin plus the number of turns still running. When the count reaches zero
/// the driver stops the process gracefully: the CLI has nothing left to do.
struct Input {
    stdin: Option<ChildStdin>,
    open_turns: usize,
}

/// A running CLI process and its normalized event stream.
pub struct Process {
    input: Arc<Mutex<Input>>,
    stop_tx: mpsc::UnboundedSender<StopMode>,
    stopped: Arc<std::sync::atomic::AtomicBool>,
    events: SyncMutex<Option<mpsc::UnboundedReceiver<SunEvent>>>,
    log: Arc<SyncMutex<Vec<SunEvent>>>,
    /// OS process id.
    pub pid: Option<u32>,
}

impl Process {
    /// Spawns `cmd` and starts reading it. `first_input` (one line, without
    /// the newline) is written to stdin; with `None` stdin is closed. Either
    /// way the session starts with one open turn. `secrets` are redacted from
    /// every event.
    pub async fn spawn(
        cli: &'static str,
        mut cmd: Command,
        normalizer: impl Normalizer,
        first_input: Option<String>,
        secrets: Vec<String>,
    ) -> Result<Self> {
        cmd.stdin(if first_input.is_some() {
            std::process::Stdio::piped()
        } else {
            std::process::Stdio::null()
        })
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        // Group leader, so a stop also reaches the CLI's own children.
        .process_group(0);
        let mut child = cmd
            .spawn()
            .map_err(|e| Error::Unavailable(format!("cannot start {cli}: {e}")))?;
        let pid = child.id();
        let (Some(stdout), Some(mut stderr)) = (child.stdout.take(), child.stderr.take()) else {
            return Err(Error::Unavailable(format!("{cli}: no stdout/stderr pipe")));
        };
        let input = Arc::new(Mutex::new(Input {
            stdin: child.stdin.take(),
            open_turns: 1,
        }));
        if let Some(line) = first_input {
            write_line(&mut input.lock().await.stdin, &line, cli).await?;
        }

        // Waiter: owns the child, applies stop requests, reports the exit code.
        let (stop_tx, mut stop_rx) = mpsc::unbounded_channel::<StopMode>();
        let waiter_input = Arc::clone(&input);
        let exits_on_eof = normalizer.exits_on_eof();
        let waiter = tokio::spawn(async move {
            let mut kill_at: Option<Instant> = None;
            let code = loop {
                tokio::select! {
                    status = child.wait() => break status.ok().and_then(|s| s.code()),
                    Some(mode) = stop_rx.recv() => match mode {
                        StopMode::Kill => kill(&mut child, pid).await,
                        StopMode::Graceful => {
                            let mut input = waiter_input.lock().await;
                            input.stdin = None;
                            let busy = input.open_turns > 0;
                            drop(input);
                            if exits_on_eof || busy {
                                kill_at.get_or_insert(Instant::now() + GRACE);
                            } else {
                                kill(&mut child, pid).await;
                            }
                        }
                    },
                    () = tokio::time::sleep_until(kill_at.unwrap_or_else(Instant::now)), if kill_at.is_some() => {
                        kill(&mut child, pid).await;
                        kill_at = None;
                    }
                }
            };
            // Background jobs the CLI left behind would hold stdout open.
            if let Some(pid) = pid {
                kill_group(pid).await;
            }
            code
        });

        let stderr_task = tokio::spawn(async move {
            let mut tail = Vec::new();
            let mut chunk = [0u8; 1024];
            while let Ok(n @ 1..) = stderr.read(&mut chunk).await {
                tail.extend_from_slice(&chunk[..n]);
                let excess = tail.len().saturating_sub(STDERR_TAIL);
                tail.drain(..excess);
            }
            String::from_utf8_lossy(&tail).into_owned()
        });

        // Reader: stdout lines → events, then the closing events.
        let (events_tx, events_rx) = mpsc::unbounded_channel();
        let log = Arc::new(SyncMutex::new(Vec::new()));
        let stopped = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (reader_input, reader_log, reader_stopped, reader_stop_tx) = (
            Arc::clone(&input),
            Arc::clone(&log),
            Arc::clone(&stopped),
            stop_tx.clone(),
        );
        let mut normalizer = normalizer;
        tokio::spawn(async move {
            let secrets: Vec<&str> = secrets.iter().map(String::as_str).collect();
            let emit = |events: Vec<SunEvent>| {
                if let Ok(mut log) = reader_log.lock() {
                    log.extend(events.iter().cloned());
                }
                for event in events {
                    let _ = events_tx.send(event);
                }
            };
            let mut lines = BufReader::new(stdout).lines();
            while let Ok(Some(raw)) = lines.next_line().await {
                let mut out = Vec::new();
                let turn_ended = feed(&mut normalizer, &raw, &secrets, &mut out);
                let replies = normalizer.replies();
                if !replies.is_empty() {
                    let mut input = reader_input.lock().await;
                    for reply in replies {
                        // A closed stdin means the session is ending anyway.
                        let _ = write_line(&mut input.stdin, &reply, cli).await;
                    }
                }
                if turn_ended {
                    let mut input = reader_input.lock().await;
                    input.open_turns = input.open_turns.saturating_sub(1);
                    if input.open_turns == 0 {
                        let _ = reader_stop_tx.send(StopMode::Graceful);
                    }
                }
                emit(out);
            }
            let code = waiter.await.ok().flatten();
            let stderr = stderr_task.await.unwrap_or_default();
            let open_turns = reader_input.lock().await.open_turns;
            let stopped = reader_stopped.load(std::sync::atomic::Ordering::SeqCst);
            emit(finish(cli, open_turns, stopped, code, &stderr, &secrets));
        });

        Ok(Self {
            input,
            stop_tx,
            stopped,
            events: SyncMutex::new(Some(events_rx)),
            log,
            pid,
        })
    }

    /// Writes one more input line (a new user turn). Fails once the session
    /// has finished its turns or was stopped.
    pub async fn send_line(&self, line: &str, cli: &str) -> Result<()> {
        let mut input = self.input.lock().await;
        if input.stdin.is_none() {
            return Err(Error::Unavailable(format!(
                "{cli} session is no longer accepting input"
            )));
        }
        write_line(&mut input.stdin, line, cli).await?;
        input.open_turns += 1;
        Ok(())
    }

    /// Writes a line that starts no turn (e.g. an ACP notification).
    pub async fn notify(&self, line: &str, cli: &str) -> Result<()> {
        write_line(&mut self.input.lock().await.stdin, line, cli).await
    }

    /// Stops the process and its process group. Graceful closes stdin and
    /// kills after [`GRACE`]; kill is immediate. Idempotent.
    pub fn stop(&self, how: StopMode) {
        self.stopped
            .store(true, std::sync::atomic::Ordering::SeqCst);
        // The waiter is gone once the process exited: nothing left to stop.
        let _ = self.stop_tx.send(how);
    }

    /// The event stream. The first call gets every event; later calls get an
    /// empty stream (the contract is "exactly once").
    pub fn events(&self) -> BoxStream<'static, SunEvent> {
        stream(self.events.lock().ok().and_then(|mut slot| slot.take()))
    }

    /// Every event emitted so far, for `export`.
    pub fn log(&self) -> Vec<SunEvent> {
        self.log.lock().map(|log| log.clone()).unwrap_or_default()
    }
}

/// SIGKILLs the child's process group (the CLI and everything it started),
/// then the child itself in case the group signal failed.
async fn kill(child: &mut tokio::process::Child, pid: Option<u32>) {
    if let Some(pid) = pid {
        kill_group(pid).await;
    }
    let _ = child.start_kill();
}

/// SIGKILLs process group `pgid`. Uses the `kill` binary because the
/// workspace forbids `unsafe` and so direct syscalls.
pub async fn kill_group(pgid: u32) {
    let _ = Command::new("kill")
        .args(["-KILL", "--", &format!("-{pgid}")])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .await;
}

async fn write_line(stdin: &mut Option<ChildStdin>, line: &str, cli: &str) -> Result<()> {
    let Some(pipe) = stdin.as_mut() else {
        return Err(Error::Unavailable(format!("{cli}: stdin is closed")));
    };
    let mut bytes = line.as_bytes().to_vec();
    bytes.push(b'\n');
    let result = async {
        pipe.write_all(&bytes).await?;
        pipe.flush().await
    }
    .await;
    result.map_err(|e| Error::Unavailable(format!("{cli}: writing stdin: {e}")))
}

/// An event receiver as a stream; `None` is an empty stream.
pub fn stream(rx: Option<mpsc::UnboundedReceiver<SunEvent>>) -> BoxStream<'static, SunEvent> {
    Box::pin(Receiver(rx))
}

/// The live sessions of one adapter, by Sun's session id.
// ponytail: sessions stay for the adapter's lifetime (export needs them after
// exit); add a forget() when the engine tracks cleanup.
pub struct Sessions<T>(SyncMutex<HashMap<SessionId, Arc<T>>>);

impl<T> Default for Sessions<T> {
    fn default() -> Self {
        Self(SyncMutex::new(HashMap::new()))
    }
}

impl<T> Sessions<T> {
    /// Registers a started session.
    pub fn insert(&self, id: SessionId, session: T) -> Arc<T> {
        let session = Arc::new(session);
        if let Ok(mut sessions) = self.0.lock() {
            sessions.insert(id, Arc::clone(&session));
        }
        session
    }

    /// The session `id` of adapter `cli`.
    pub fn get(&self, cli: &str, id: &SessionId) -> Result<Arc<T>> {
        self.0
            .lock()
            .ok()
            .and_then(|sessions| sessions.get(id).cloned())
            .ok_or_else(|| Error::NotFound(format!("{cli} session {id}")))
    }
}

/// How long `start` waits for the CLI to report its own session id.
pub const STARTUP: Duration = Duration::from_secs(30);

/// The CLI's own session id once its output reported it (Codex
/// `thread.started`, ACP `session/new`); `None` if the process ended first
/// (the normalizer, which owns the sender, is dropped) or [`STARTUP`] ran out.
pub async fn native_id(rx: &mut watch::Receiver<Option<String>>) -> Option<String> {
    let id = tokio::time::timeout(STARTUP, rx.wait_for(Option::is_some)).await;
    id.ok()?.ok()?.clone()
}

/// `path` relative to `cwd` when it lies inside it, else as-is.
pub fn relative(cwd: Option<&Path>, path: &str) -> String {
    cwd.and_then(|cwd| Path::new(path).strip_prefix(cwd).ok())
        .map_or_else(|| path.to_owned(), |p| p.to_string_lossy().into_owned())
}

/// The program an adapter starts for `binary` with session env `env`: inside
/// an exec environment ([`EXEC_SHIMS_ENV`] set) the shim of the same file
/// name, so the CLI runs inside the environment even when `binary` is an
/// absolute path; a CLI without a shim then fails to start.
pub fn program(binary: &Path, env: &BTreeMap<String, String>) -> PathBuf {
    match (env.get(EXEC_SHIMS_ENV), binary.file_name()) {
        (Some(shims), Some(name)) => Path::new(shims).join(name),
        _ => binary.to_path_buf(),
    }
}

/// What a readiness check needs to know about one CLI.
pub struct Probe<'a> {
    /// Product name for messages, e.g. `Codex`.
    pub name: &'a str,
    /// The binary to run.
    pub binary: &'a Path,
    /// Adapter id: its tested versions are in `fixtures/versions.txt`.
    pub cli: &'a str,
    /// How to install the CLI; `<install>@<pinned>` installs the fallback.
    pub install: &'a str,
    /// Arguments of the login check; empty when the CLI needs no login.
    pub login_args: &'a [&'a str],
    /// Reads the login check's output.
    pub logged_in: fn(&std::process::Output) -> bool,
    /// How to log in.
    pub login: &'a str,
}

/// Runs `<binary> --version` and the login check. Uses no quota.
// ponytail: checks the local machine; the host matters once sun-host runs
// adapters on remote hosts and forwards the check.
pub async fn readiness(probe: &Probe<'_>) -> Readiness {
    readiness_within(probe, PROBE_CAP).await
}

/// [`readiness`] with each command capped at `cap`.
async fn readiness_within(probe: &Probe<'_>, cap: Duration) -> Readiness {
    let name = probe.name;
    let version = match run_within(probe.binary, &["--version"], cap).await {
        Ok(out) => parse_version(&String::from_utf8_lossy(&out.stdout)),
        // It started but didn't answer in time (a loaded host, a slow npm
        // shim): installed, not ready yet. Not logged in keeps it off the
        // scheduler and listed as `needs_login` with "Check again"; the next
        // re-probe settles it.
        Err(e) if e.kind() == std::io::ErrorKind::TimedOut => {
            return Readiness {
                installed: true,
                version: None,
                logged_in: false,
                detail: Some(format!(
                    "{name} did not answer `--version` within {} s; the host may be busy. \
                     Sun checks again in a few minutes",
                    cap.as_secs()
                )),
            };
        }
        Err(e) => {
            return Readiness {
                installed: false,
                version: None,
                logged_in: false,
                detail: Some(format!(
                    "{name} not found ({e}). Install it: {}",
                    probe.install
                )),
            };
        }
    };
    let logged_in = probe.login_args.is_empty()
        || run_within(probe.binary, probe.login_args, cap)
            .await
            .is_ok_and(|out| (probe.logged_in)(&out));
    let detail = if !logged_in {
        Some(format!("{name} is not logged in. Run: {}", probe.login))
    } else {
        match &version {
            Some(v) if is_tested(probe.cli, v) => None,
            Some(v) => Some(untested(probe, v)),
            None => Some(format!("{name} did not report a version")),
        }
    };
    Readiness {
        installed: true,
        version,
        logged_in,
        detail,
    }
}

/// `fixtures/versions.txt`: per CLI, the pinned known-good version, then
/// older tested ones. The weekly `cli-matrix` CI job adds versions.
const VERSIONS: &str = include_str!("../fixtures/versions.txt");

/// The versions of adapter `cli` the contract tests cover, pinned first.
pub fn tested_versions(cli: &str) -> Vec<&'static str> {
    VERSIONS
        .lines()
        .find_map(|line| {
            let mut words = line.split_whitespace();
            (words.next() == Some(cli)).then(|| words.collect())
        })
        .unwrap_or_default()
}

/// Same `major.minor` as a tested version: patch releases count as tested.
pub fn is_tested(cli: &str, version: &str) -> bool {
    tested_versions(cli).iter().any(|t| {
        let family = t.rsplit_once('.').map_or(*t, |(family, _)| family);
        version
            .strip_prefix(family)
            .is_some_and(|rest| rest.starts_with('.'))
    })
}

/// The readiness warning for an untested version, naming the pinned fallback.
fn untested(probe: &Probe<'_>, version: &str) -> String {
    let name = probe.name;
    match tested_versions(probe.cli).first() {
        Some(pinned) => format!(
            "{name} {version} is untested. If it misbehaves, install the known-good \
             {pinned}: {}@{pinned}",
            probe.install
        ),
        None => format!("{name} {version} is untested"),
    }
}

/// `2.1.291 (Claude Code)`, `codex-cli 0.160.0`, `1.18.34` → the version.
pub fn parse_version(out: &str) -> Option<String> {
    out.split_whitespace()
        .find(|word| word.starts_with(|c: char| c.is_ascii_digit()))
        .map(str::to_owned)
}

/// How long a readiness or effort probe command may take.
const PROBE_CAP: Duration = Duration::from_secs(15);

/// Runs `binary args` to completion ([`PROBE_CAP`]): readiness and effort probes.
pub(crate) async fn run(binary: &Path, args: &[&str]) -> std::io::Result<std::process::Output> {
    run_within(binary, args, PROBE_CAP).await
}

/// [`run`] capped at `cap`; running past it is `ErrorKind::TimedOut`.
async fn run_within(
    binary: &Path,
    args: &[&str],
    cap: Duration,
) -> std::io::Result<std::process::Output> {
    tokio::time::timeout(cap, Command::new(binary).args(args).output())
        .await
        .map_err(|_| std::io::Error::from(std::io::ErrorKind::TimedOut))?
}

/// An mpsc receiver as a `Stream` (saves a tokio-stream dependency).
struct Receiver(Option<mpsc::UnboundedReceiver<SunEvent>>);

impl Stream for Receiver {
    type Item = SunEvent;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<SunEvent>> {
        match self.0.as_mut() {
            Some(rx) => rx.poll_recv(cx),
            None => Poll::Ready(None),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn args_hash_is_stable_and_order_free() {
        let a: serde_json::Value = serde_json::from_str(r#"{"a":1,"b":"x"}"#).unwrap();
        let b: serde_json::Value = serde_json::from_str(r#"{"b":"x","a":1}"#).unwrap();
        assert_eq!(args_hash(&a), args_hash(&b));
        // FNV-1a("null"), checked against a reference implementation.
        assert_eq!(args_hash(&serde_json::Value::Null), 0x5b9b_c4ba_5281_08e4);
    }

    #[test]
    fn exec_env_shims_replace_the_binary() {
        let bare = BTreeMap::new();
        assert_eq!(program(Path::new("claude"), &bare), Path::new("claude"));
        let env = BTreeMap::from([(EXEC_SHIMS_ENV.to_owned(), "/d/env/S".to_owned())]);
        for binary in ["claude", "/opt/bin/claude"] {
            assert_eq!(
                program(Path::new(binary), &env),
                Path::new("/d/env/S/claude")
            );
        }
    }

    #[test]
    fn parses_versions() {
        for (out, want) in [
            ("2.1.291 (Claude Code)\n", Some("2.1.291")),
            ("codex-cli 0.160.0\n", Some("0.160.0")),
            ("1.18.34\n", Some("1.18.34")),
            ("", None),
        ] {
            assert_eq!(parse_version(out).as_deref(), want, "{out}");
        }
    }

    /// BLO-170: a CLI that doesn't answer `--version` in time stays listed
    /// as not ready (`needs_login` with the reason), not dropped as missing.
    #[tokio::test]
    async fn a_probe_timeout_is_not_ready_not_uninstalled() {
        use std::os::unix::fs::PermissionsExt;
        use sun_core::{Availability, Presence, RuntimeState};

        let dir = tempfile::tempdir().unwrap();
        let slow = dir.path().join("slow-cli");
        std::fs::write(&slow, "#!/bin/sh\nsleep 5\n").unwrap();
        std::fs::set_permissions(&slow, std::fs::Permissions::from_mode(0o755)).unwrap();
        let probe = Probe {
            name: "Slow",
            binary: &slow,
            cli: "slow",
            install: "npm install -g slow",
            login_args: &[],
            logged_in: |_| true,
            login: "slow login",
        };
        let ready = readiness_within(&probe, Duration::from_millis(200)).await;
        assert!(ready.installed && !ready.logged_in, "{ready:?}");
        assert_eq!(ready.version, None);
        let detail = ready.detail.as_deref().unwrap();
        assert!(detail.contains("did not answer `--version`"), "{detail}");
        assert!(!detail.contains("not found"), "{detail}");
        let availability = Availability::of(&ready, false);
        assert_eq!(availability, Availability::NeedsLogin);
        assert_eq!(
            RuntimeState::derive(Presence::Online, availability, 0),
            RuntimeState::NeedsLogin
        );

        let missing = Probe {
            binary: &dir.path().join("not-there"),
            ..probe
        };
        let gone = readiness_within(&missing, Duration::from_millis(200)).await;
        assert!(!gone.installed, "{gone:?}");
    }

    #[test]
    fn truncate_counts_chars() {
        assert_eq!(truncate("  héllo  ", 10), "héllo");
        assert_eq!(truncate("héllo", 2), "hé…");
    }
}
