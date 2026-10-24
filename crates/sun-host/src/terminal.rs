//! Terminals (`pty.open`): the host user's shell on a PTY in a member's
//! editor worktree, its bytes on a socket of its own that the host dials to
//! the kernel (`GET /host/pty`), so a flood of output never stalls the
//! control socket.
//!
//! The shell runs as the host's service user (`sun` on Linux installs) with
//! that user's files, network and environment: a terminal is the host.
//! Its environment is the host's own, as task sessions get it (so `claude`
//! is on `PATH` the same way), plus `TERM=xterm-256color`. When the socket
//! closes, or the kernel goes silent for [`HostConfig::liveness`], the
//! shell's whole session is killed; when the shell exits, its session goes
//! too and the exit code is the socket's last message. Nothing typed or
//! shown is logged.
//!
//! [`HostConfig::liveness`]: crate::HostConfig::liveness

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use sun_core::host_protocol::{EditorTree, MAX_PTYS, PtyControl, PtyHello};
use sun_core::{Error, Result};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::Command;
use tokio::sync::Notify;
use tokio_tungstenite::tungstenite::Message;

use crate::host::Host;
use crate::process::kill_group;

/// Output read from the PTY per message.
const CHUNK: usize = 64 * 1024;
/// After the shell exits, how long output its leftovers still write is sent.
const DRAIN: Duration = Duration::from_secs(1);

impl Host {
    /// Spawns the shell for `pty.open` and dials its socket; `Ok` once the
    /// socket said hello. The shell is killed when anything fails.
    pub(crate) async fn open_pty(
        self: &Arc<Self>,
        pty_id: String,
        tree: EditorTree,
        cols: u16,
        rows: u16,
    ) -> Result<()> {
        let valid =
            (1..=64).contains(&pty_id.len()) && pty_id.bytes().all(|b| b.is_ascii_alphanumeric());
        if !valid {
            return Err(Error::InvalidInput(
                "pty_id: 1-64 letters and digits".into(),
            ));
        }
        PtyControl::check_size(cols, rows)?;
        let credentials = self
            .credentials
            .get()
            .ok_or_else(|| Error::Unavailable("not connected to a kernel".into()))?;
        let root = self.worktrees.editor_root(&tree)?;
        {
            let mut live = self.ptys();
            if live.contains_key(&pty_id) {
                return Err(Error::InvalidInput(format!("terminal {pty_id} is open")));
            }
            if live.values().filter(|m| **m == tree.member_id).count() >= MAX_PTYS {
                return Err(Error::InvalidInput(format!(
                    "at most {MAX_PTYS} terminals per member on a host; close one first (a closed tab keeps its terminal for 30 s)"
                )));
            }
            live.insert(pty_id.clone(), tree.member_id);
        }
        let opened = async {
            let (pty, child) = spawn(&root, cols, rows)?;
            let url = format!("{}/pty", credentials.kernel_url.trim_end_matches('/'));
            let hello = PtyHello {
                host_id: credentials.host_id,
                host_key: credentials.host_key.clone(),
                pty_id: pty_id.clone(),
            };
            let dial = async {
                let (mut ws, _) = tokio_tungstenite::connect_async(url.as_str()).await?;
                let hello = serde_json::to_string(&hello).unwrap_or_default();
                ws.send(Message::text(hello)).await?;
                Ok::<_, tokio_tungstenite::tungstenite::Error>(ws)
            };
            let ws = match tokio::time::timeout(self.config.liveness, dial).await {
                Ok(Ok(ws)) => ws,
                failed => {
                    let mut child = child;
                    let sid = child.id();
                    end(&mut child, sid).await;
                    let why = match failed {
                        Ok(Err(e)) => e.to_string(),
                        _ => "timed out".into(),
                    };
                    return Err(Error::Unavailable(format!("terminal socket: {why}")));
                }
            };
            Ok((pty, child, ws))
        };
        let (pty, child, ws) = match opened.await {
            Ok(opened) => opened,
            Err(e) => {
                self.ptys().remove(&pty_id);
                return Err(e);
            }
        };
        let host = Arc::clone(self);
        tokio::spawn(async move {
            serve(pty, child, ws, host.config.liveness).await;
            host.ptys().remove(&pty_id);
        });
        Ok(())
    }
}

/// The shell on a new PTY of `cols`×`rows`, in `cwd`, leading its own session.
fn spawn(cwd: &Path, cols: u16, rows: u16) -> Result<(pty_process::Pty, tokio::process::Child)> {
    let failed = |e: pty_process::Error| Error::Unavailable(format!("terminal: {e}"));
    let (pty, pts) = pty_process::open().map_err(failed)?;
    pty.resize(pty_process::Size::new(rows, cols))
        .map_err(failed)?;
    let child = pty_process::Command::new(shell())
        .current_dir(cwd)
        .env("TERM", "xterm-256color")
        .env("COLORTERM", "truecolor")
        .kill_on_drop(true)
        .spawn(pts)
        .map_err(failed)?;
    Ok((pty, child))
}

/// `$SHELL`, else the login shell of `$USER` in `/etc/passwd`, else `/bin/sh`.
fn shell() -> PathBuf {
    let usable = |path: PathBuf| (path.is_absolute() && path.is_file()).then_some(path);
    let login = || {
        let user = std::env::var("USER")
            .or_else(|_| std::env::var("LOGNAME"))
            .ok()?;
        let passwd = std::fs::read_to_string("/etc/passwd").ok()?;
        passwd.lines().find_map(|line| {
            let fields: Vec<&str> = line.split(':').collect();
            (fields.len() == 7 && fields[0] == user).then(|| PathBuf::from(fields[6]))
        })
    };
    std::env::var_os("SHELL")
        .map(PathBuf::from)
        .and_then(usable)
        .or_else(|| login().and_then(usable))
        .unwrap_or_else(|| "/bin/sh".into())
}

/// Pumps one terminal until its shell exits (exit code sent, socket
/// closed) or the kernel closes the socket or goes silent (shell killed).
async fn serve<S>(
    pty: pty_process::Pty,
    mut child: tokio::process::Child,
    ws: tokio_tungstenite::WebSocketStream<S>,
    liveness: Duration,
) where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    // The session id: `child.id()` is gone once the shell is reaped.
    let sid = child.id();
    let (mut sink, mut stream) = ws.split();
    let (mut reader, mut writer) = pty.into_split();
    let exited = Notify::new();
    // Kernel → shell. Ends when the kernel is gone.
    let input = async {
        while let Ok(Some(Ok(message))) = tokio::time::timeout(liveness, stream.next()).await {
            match message {
                Message::Binary(bytes) => {
                    if writer.write_all(&bytes).await.is_err() {
                        // The shell is gone; its exit is on the way.
                        std::future::pending::<()>().await;
                    }
                }
                Message::Text(text) => {
                    if let Ok(PtyControl::Resize { cols, rows }) = serde_json::from_str(&text)
                        && PtyControl::check_size(cols, rows).is_ok()
                    {
                        let _ = writer.resize(pty_process::Size::new(rows, cols));
                    }
                }
                Message::Close(_) => return,
                _ => {} // pings are answered by tungstenite
            }
        }
    };
    // Shell → kernel, until the PTY is drained or [`DRAIN`] after the exit.
    let output = async {
        let mut buffer = vec![0; CHUNK];
        let mut deadline = None;
        loop {
            let after_exit = async {
                match deadline {
                    Some(at) => tokio::time::sleep_until(at).await,
                    None => exited.notified().await,
                }
            };
            tokio::select! {
                read = reader.read(&mut buffer) => match read {
                    // EIO on Linux once every holder of the other side is gone.
                    Ok(0) | Err(_) => return,
                    Ok(n) => {
                        if sink.send(Message::binary(buffer[..n].to_vec())).await.is_err() {
                            return;
                        }
                    }
                },
                () = after_exit => match deadline {
                    Some(_) => return,
                    None => deadline = Some(tokio::time::Instant::now() + DRAIN),
                },
            }
        }
    };
    let ended = async {
        let status = child.wait().await;
        kill_session(sid).await;
        exited.notify_one();
        status.ok().and_then(|s| s.code())
    };
    let code = tokio::select! {
        () = input => None,
        (code, ()) = async { tokio::join!(ended, output) } => Some(code),
    };
    match code {
        Some(code) => {
            let exit = serde_json::to_string(&PtyControl::Exit { code }).unwrap_or_default();
            let _ = sink.send(Message::text(exit)).await;
            let _ = sink.close().await;
        }
        None => end(&mut child, sid).await,
    }
}

/// Kills the session `sid` the shell leads and reaps the shell.
async fn end(child: &mut tokio::process::Child, sid: Option<u32>) {
    kill_session(sid).await;
    let _ = child.start_kill();
    let _ = child.wait().await;
}

/// SIGKILLs every process of the session `sid` leads: its own process group
/// and the jobs the shell put in groups of their own (`vim`, `cmd &`).
// ponytail: `pkill -s` (procps / BSD); a process that called `setsid`
// itself (a daemon) outlives the terminal, as on a desktop.
async fn kill_session(sid: Option<u32>) {
    let Some(sid) = sid else { return };
    kill_group(sid).await;
    let _ = Command::new("pkill")
        .args(["-KILL", "-s", &sid.to_string()])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await;
}
