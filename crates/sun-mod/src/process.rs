//! One running mod process: JSON-RPC 2.0 requests over its stdin, responses
//! and `event` notifications from its stdout, one message per line. Requests
//! from the mod (API 1.1) go to the host's handler and are answered on stdin.

use std::collections::HashMap;
use std::path::Path;
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use futures_core::future::BoxFuture;
use serde_json::Value;
use sun_core::{Error, Result, redact::redact_known};
use sun_mod_sdk::{EventParams, Manifest, Message, RpcError, code, method};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStderr, ChildStdin, ChildStdout, Command};
use tokio::sync::{oneshot, watch};

use crate::sandbox::Sandbox;
use crate::sandbox::proxy::{self, Proxy};

/// Longest line a mod may send; a longer one is a protocol violation.
const MAX_LINE: usize = 8 * 1024 * 1024;
/// A longer stderr line is logged in pieces of this size.
const MAX_LOG_LINE: u64 = 64 * 1024;

type Waiter = oneshot::Sender<Result<Value>>;

/// Calls in flight; `None` once the process is gone.
type Pending = Arc<Mutex<Option<HashMap<u64, Waiter>>>>;

type Stdin = Arc<tokio::sync::Mutex<ChildStdin>>;

/// Answers a request the mod sent (`method`, `params`).
pub(crate) type OnRequest =
    Box<dyn Fn(String, Value) -> BoxFuture<'static, std::result::Result<Value, RpcError>> + Send>;

/// A started mod. Dropping it kills the process.
pub(crate) struct ModProcess {
    name: String,
    pub(crate) started_at: Instant,
    stdin: Stdin,
    pending: Pending,
    next_id: AtomicU64,
    exited: watch::Receiver<bool>,
    kill: Mutex<Option<oneshot::Sender<()>>>,
    /// A sandboxed mod's only way out; stops with the process.
    _proxy: Option<Proxy>,
}

impl ModProcess {
    /// Starts the mod's entry command in `dir` with a clean environment,
    /// inside `sandbox` if given. `on_event` receives every `event`
    /// notification, `on_request` every request the mod sends.
    pub(crate) fn spawn(
        manifest: &Manifest,
        dir: &Path,
        sandbox: Option<&Sandbox>,
        on_event: impl Fn(EventParams) + Send + 'static,
        on_request: OnRequest,
    ) -> Result<Self> {
        let command = &manifest.entry.command;
        let program = if command.contains('/') {
            dir.join(command)
        } else {
            command.into()
        };
        let (mut cmd, proxy) = match sandbox {
            None => {
                let mut cmd = Command::new(program);
                cmd.args(&manifest.entry.args).env_clear();
                if let Some(home) = std::env::var_os("HOME") {
                    cmd.env("HOME", home);
                }
                (cmd, None)
            }
            Some(sandbox) => {
                let proxy = proxy::start(sandbox.network.clone()).map_err(|e| {
                    Error::Unavailable(format!("mod {}: proxy: {e}", manifest.name))
                })?;
                let cmd = sandbox.command(program, &manifest.entry.args, dir, proxy.port)?;
                (cmd, Some(proxy))
            }
        };
        cmd.current_dir(dir)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        if let Some(path) = std::env::var_os("PATH") {
            cmd.env("PATH", path);
        }
        let name = manifest.name.clone();
        let mut child = cmd
            .spawn()
            .map_err(|e| Error::Unavailable(format!("cannot start mod {name}: {e}")))?;
        let (Some(stdin), Some(stdout)) = (child.stdin.take(), child.stdout.take()) else {
            return Err(Error::Unavailable(format!("mod {name}: no stdio")));
        };
        if let Some(stderr) = child.stderr.take() {
            tokio::spawn(log_stderr(name.clone(), stderr));
        }
        let pending: Pending = Arc::new(Mutex::new(Some(HashMap::new())));
        let stdin: Stdin = Arc::new(tokio::sync::Mutex::new(stdin));
        let (exited_tx, exited) = watch::channel(false);
        let (kill, kill_rx) = oneshot::channel();
        tokio::spawn(pump(
            name.clone(),
            child,
            stdout,
            Handlers {
                pending: pending.clone(),
                on_event,
                on_request,
                stdin: stdin.clone(),
            },
            kill_rx,
            exited_tx,
        ));
        Ok(Self {
            name,
            started_at: Instant::now(),
            stdin,
            pending,
            next_id: AtomicU64::new(1),
            exited,
            kill: Mutex::new(Some(kill)),
            _proxy: proxy,
        })
    }

    /// Sends a request and waits up to `timeout` for its response. A timeout
    /// kills the process (a mod that stops answering is wedged) and returns
    /// `Unavailable`, as does a process that exits first.
    pub(crate) async fn request(
        &self,
        method: &str,
        params: Value,
        timeout: Duration,
    ) -> Result<Value> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        match lock(&self.pending).as_mut() {
            Some(waiters) => waiters.insert(id, tx),
            None => return Err(self.gone()),
        };
        let written = send(&self.stdin, &Message::request(id, method, params)).await;
        if written.is_err() {
            self.forget(id);
            return Err(self.gone());
        }
        match tokio::time::timeout(timeout, rx).await {
            Ok(Ok(outcome)) => outcome,
            Ok(Err(_)) => Err(self.gone()),
            Err(_) => {
                self.forget(id);
                self.kill();
                Err(Error::Unavailable(format!(
                    "mod {}: {method} timed out after {timeout:?}",
                    self.name
                )))
            }
        }
    }

    /// False once it exited or a kill was requested.
    pub(crate) fn is_alive(&self) -> bool {
        !*self.exited.borrow() && lock(&self.kill).is_some()
    }

    pub(crate) fn in_flight(&self) -> usize {
        lock(&self.pending).as_ref().map_or(0, HashMap::len)
    }

    /// Resolves once the process has exited and been reaped.
    pub(crate) async fn exited(&self) {
        let mut rx = self.exited.clone();
        let _ = rx.wait_for(|done| *done).await;
    }

    /// Kills the process now.
    pub(crate) fn kill(&self) {
        if let Some(kill) = lock(&self.kill).take() {
            let _ = kill.send(());
        }
    }

    /// Asks the mod to `shutdown`, waits up to `grace`, then kills it.
    pub(crate) async fn stop(&self, grace: Duration) {
        let _ = self.request(method::SHUTDOWN, Value::Null, grace).await;
        let _ = tokio::time::timeout(grace, self.exited()).await;
        self.kill();
        self.exited().await;
    }

    fn forget(&self, id: u64) {
        if let Some(waiters) = lock(&self.pending).as_mut() {
            waiters.remove(&id);
        }
    }

    fn gone(&self) -> Error {
        Error::Unavailable(format!("mod {} exited", self.name))
    }
}

/// Owns the child: routes its stdout until EOF, a protocol violation or a
/// kill, then kills and reaps it and fails every call still in flight.
async fn pump<E: Fn(EventParams)>(
    name: String,
    mut child: Child,
    stdout: ChildStdout,
    handlers: Handlers<E>,
    mut kill: oneshot::Receiver<()>,
    exited: watch::Sender<bool>,
) {
    let pending = handlers.pending.clone();
    let mut reader = BufReader::new(stdout);
    let mut buf = Vec::new();
    loop {
        buf.clear();
        let mut limited = (&mut reader).take(MAX_LINE as u64 + 1);
        let read = tokio::select! {
            read = limited.read_until(b'\n', &mut buf) => read,
            _ = &mut kill => break,
        };
        match read {
            Ok(0) | Err(_) => break,
            Ok(_) if buf.len() > MAX_LINE => break,
            Ok(_) => {}
        }
        if route(&name, &buf, &handlers).is_err() {
            break; // ponytail: violations are dropped silently until mods have a log sink
        }
    }
    let _ = child.start_kill();
    let _ = child.wait().await;
    // Mark it dead before failing the callers, so a caller retrying on its
    // error already sees a dead process and starts a new one.
    let _ = exited.send(true);
    if let Some(waiters) = lock(&pending).take() {
        for (_, waiter) in waiters {
            let _ = waiter.send(Err(Error::Unavailable(format!("mod {name} exited"))));
        }
    }
}

/// Logs the mod's stderr, one redacted line per `tracing` event (target
/// `mod`), until it closes. Never a raw copy: a mod may print its secrets.
async fn log_stderr(name: String, stderr: ChildStderr) {
    let mut reader = BufReader::new(stderr);
    let mut buf = Vec::new();
    loop {
        buf.clear();
        // ponytail: a line over MAX_LOG_LINE is split, so a secret straddling
        // the cut escapes the known-value match; buffer across cuts if mods
        // ever log that much in one line.
        let mut limited = (&mut reader).take(MAX_LOG_LINE);
        match limited.read_until(b'\n', &mut buf).await {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }
        let line = redact_known(String::from_utf8_lossy(&buf).trim_end());
        tracing::info!(target: "mod", "mod {name}: {line}");
    }
}

/// Where the mod's messages go.
struct Handlers<E> {
    pending: Pending,
    on_event: E,
    on_request: OnRequest,
    /// Answers to the mod's requests go here.
    stdin: Stdin,
}

/// Writes one message line to the mod.
async fn send(stdin: &Stdin, msg: &Message) -> std::io::Result<()> {
    let mut line = serde_json::to_vec(msg)?;
    line.push(b'\n');
    let mut stdin = stdin.lock().await;
    stdin.write_all(&line).await?;
    stdin.flush().await
}

/// Handles one line. `Err` means the mod broke the protocol.
fn route<E: Fn(EventParams)>(
    name: &str,
    line: &[u8],
    handlers: &Handlers<E>,
) -> std::result::Result<(), ()> {
    let (pending, on_event) = (&handlers.pending, &handlers.on_event);
    if line.iter().all(u8::is_ascii_whitespace) {
        return Ok(());
    }
    let msg: Message = serde_json::from_slice(line).map_err(|_| ())?;
    if msg.jsonrpc != "2.0" {
        return Err(());
    }
    match (&msg.method, &msg.id) {
        (Some(m), None) if m == method::EVENT => {
            let params = msg.params.unwrap_or(Value::Null);
            on_event(serde_json::from_value(params).map_err(|_| ())?);
            Ok(())
        }
        (None, Some(id)) => {
            let id = id.as_u64().ok_or(())?;
            let waiter = lock(pending).as_mut().and_then(|w| w.remove(&id));
            if let Some(waiter) = waiter {
                let _ = waiter.send(msg.into_outcome().map_err(|e| mod_error(name, e)));
            }
            Ok(())
        }
        (Some(method), Some(id)) => {
            // API 1.1: answered off the read loop, so the mod's own calls
            // keep flowing while the kernel works on its request.
            let answer = (handlers.on_request)(method.clone(), msg.params.unwrap_or(Value::Null));
            let (id, stdin) = (id.clone(), handlers.stdin.clone());
            tokio::spawn(async move {
                let _ = send(&stdin, &Message::response(id, answer.await)).await;
            });
            Ok(())
        }
        _ => Err(()),
    }
}

/// Maps a mod's error to the boundary error. Only `UNAVAILABLE` is
/// retryable; a refusal carrying `data.http_status` keeps the status
/// ([`Error::Refused`]).
fn mod_error(name: &str, e: RpcError) -> Error {
    let msg = redact_known(&format!("mod {name}: {}", e.message));
    match (e.code, e.http_status()) {
        (code::UNAVAILABLE, _) => Error::Unavailable(msg),
        (code::METHOD_NOT_FOUND, _) => Error::NotFound(msg),
        (code::PARSE_ERROR | code::INVALID_REQUEST | code::INTERNAL_ERROR, _) => {
            Error::Protocol(msg)
        }
        (_, Some(http_status)) => Error::Refused {
            http_status,
            message: msg,
        },
        // ponytail: an action that ran and failed has no core variant yet;
        // InvalidInput (not retryable) until sun-core adds one.
        _ => Error::InvalidInput(msg),
    }
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mod_errors_keep_the_http_status_as_data() {
        let refused = RpcError::failed("POST /x: HTTP 401 Unauthorized").with_http_status(401);
        assert!(matches!(
            mod_error("gitlab", refused),
            Error::Refused {
                http_status: 401,
                ..
            }
        ));
        // Only the data counts: a 401 in the text alone is a plain refusal.
        let text_only = RpcError::failed("POST /x: HTTP 401 Unauthorized");
        assert!(matches!(
            mod_error("gitlab", text_only),
            Error::InvalidInput(_)
        ));
        let busy = RpcError::unavailable("busy").with_http_status(503);
        assert!(matches!(mod_error("gitlab", busy), Error::Unavailable(_)));
    }
}
