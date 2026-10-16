//! The WebSocket to the kernel: enrolment, the reconnect loop with
//! jittered exponential back-off, `hello` + resend, and frame pumping.

use std::hash::{BuildHasher, RandomState};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use sun_core::host_protocol::{HOST_PROTOCOL_VERSION, HostFrame, KernelFrame};
use sun_core::{Error, HostId, Result};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;

/// How often task folder disk use is measured and sent (`du` is not free).
const DISK_EVERY: Duration = Duration::from_secs(60);

use crate::buffer::io;
use crate::host::Host;

/// How long a one-shot connection (enrolment, status, leave) waits for the
/// kernel's answer.
const ENROLL_TIMEOUT: Duration = Duration::from_secs(30);

/// What a host keeps from enrolment, in `<data_dir>/host.json` (mode 0600).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Credentials {
    /// The kernel's host endpoint, e.g. `wss://sun.example.com/host`.
    pub kernel_url: String,
    /// This host's id.
    pub host_id: HostId,
    /// This host's secret key.
    pub host_key: String,
}

impl Credentials {
    fn path(data_dir: &Path) -> PathBuf {
        data_dir.join("host.json")
    }

    /// Loads the credentials saved by [`enroll`].
    pub fn load(data_dir: &Path) -> Result<Self> {
        let path = Self::path(data_dir);
        let text = std::fs::read_to_string(&path).map_err(|e| {
            Error::NotFound(format!(
                "{}: {e} (enrol first: sun host join --kernel <kernel-url> <token>)",
                path.display()
            ))
        })?;
        serde_json::from_str(&text)
            .map_err(|e| Error::InvalidInput(format!("{}: {e}", path.display())))
    }

    fn save(&self, data_dir: &Path) -> Result<()> {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        std::fs::create_dir_all(data_dir).map_err(io(data_dir))?;
        let path = Self::path(data_dir);
        let json = serde_json::to_vec_pretty(self)
            .map_err(|e| Error::Protocol(format!("encode credentials: {e}")))?;
        std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&path)
            .and_then(|mut f| f.write_all(&json))
            .map_err(io(&path))
    }

    /// Deletes the saved credentials (`sun host leave`); gone already is fine.
    pub fn remove(data_dir: &Path) -> Result<()> {
        let path = Self::path(data_dir);
        match std::fs::remove_file(&path) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(io(&path)(e)),
            _ => Ok(()),
        }
    }
}

/// `sun host join <token>`: trades the single-use enrolment `token` for a
/// host key at `kernel_url` and saves both in `data_dir`.
pub async fn enroll(
    kernel_url: &str,
    token: &str,
    name: &str,
    data_dir: &Path,
) -> Result<Credentials> {
    let enroll = HostFrame::Enroll {
        token: token.into(),
        name: name.into(),
    };
    let answer = ask(kernel_url, &enroll, "the enrolment").await?;
    let KernelFrame::Enrolled { host_id, host_key } = answer else {
        return Err(Error::Protocol(format!(
            "expected `enrolled`, got {answer:?}"
        )));
    };
    let credentials = Credentials {
        kernel_url: kernel_url.into(),
        host_id,
        host_key,
    };
    credentials.save(data_dir)?;
    Ok(credentials)
}

/// What the kernel says about an enrolled host.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostStatus {
    /// The kernel holds a session connection of this host.
    pub connected: bool,
    /// The workspaces whose runs may go to it, by name.
    pub workspaces: Vec<String>,
}

/// Asks the kernel whether it sees this host connected.
pub async fn status(credentials: &Credentials) -> Result<HostStatus> {
    let frame = HostFrame::Status {
        host_id: credentials.host_id,
        host_key: credentials.host_key.clone(),
    };
    match ask(&credentials.kernel_url, &frame, "the status check").await? {
        KernelFrame::Status {
            connected,
            workspaces,
        } => Ok(HostStatus {
            connected,
            workspaces,
        }),
        other => Err(Error::Protocol(format!("expected `status`, got {other:?}"))),
    }
}

/// Detaches this host from every workspace (`sun host leave`); the kernel
/// keeps its record, so past runs still name it.
pub async fn leave(credentials: &Credentials) -> Result<()> {
    let frame = HostFrame::Leave {
        host_id: credentials.host_id,
        host_key: credentials.host_key.clone(),
    };
    match ask(&credentials.kernel_url, &frame, "the leave").await? {
        KernelFrame::Left => Ok(()),
        other => Err(Error::Protocol(format!("expected `left`, got {other:?}"))),
    }
}

/// One-shot connection: sends `frame`, returns the kernel's one answer. A
/// kernel that refuses (bad key, unknown frame) closes without a word.
async fn ask(kernel_url: &str, frame: &HostFrame, what: &str) -> Result<KernelFrame> {
    let (mut ws, _) = tokio_tungstenite::connect_async(kernel_url)
        .await
        .map_err(unavailable)?;
    ws.send(text(frame)?).await.map_err(unavailable)?;
    let answer = tokio::time::timeout(ENROLL_TIMEOUT, async {
        while let Some(msg) = ws.next().await {
            if let Message::Text(t) = msg.map_err(unavailable)? {
                return parse(&t);
            }
        }
        Err(Error::Unavailable(format!("kernel closed {what}")))
    })
    .await
    .map_err(|_| Error::Unavailable(format!("kernel did not answer {what}")))??;
    let _ = ws.close(None).await;
    Ok(answer)
}

impl Host {
    /// Runs the host until the task is dropped: keeps a connection to the
    /// kernel open (reconnecting with jittered back-off) and samples session
    /// resources every `stats_every`.
    pub async fn run(self: Arc<Self>, credentials: Credentials) {
        let stats = async {
            let mut tick = tokio::time::interval(self.config.stats_every);
            let mut last_disk: Option<tokio::time::Instant> = None;
            loop {
                tick.tick().await;
                self.sample_stats().await;
                if last_disk.is_none_or(|at| at.elapsed() >= DISK_EVERY) {
                    self.sample_disk().await;
                    last_disk = Some(tokio::time::Instant::now());
                }
            }
        };
        let connect = async {
            let mut attempt = 0;
            loop {
                match self.serve(&credentials).await {
                    Ok(()) => attempt = 0,
                    Err(e) => eprintln!("sun-host: kernel connection: {e}"),
                }
                let delay = backoff(attempt, self.config.backoff_min, self.config.backoff_max);
                tokio::time::sleep(delay).await;
                attempt = attempt.saturating_add(1);
            }
        };
        tokio::join!(stats, connect);
    }

    /// One connection. `Err` if it never got as far as `hello`; `Ok` once a
    /// connection that said `hello` ends.
    async fn serve(self: &Arc<Self>, credentials: &Credentials) -> Result<()> {
        let capabilities = self.capabilities(credentials.host_id).await;
        let (ws, _) = tokio_tungstenite::connect_async(credentials.kernel_url.as_str())
            .await
            .map_err(unavailable)?;
        let (mut sink, mut stream) = ws.split();
        let (tx, mut rx) = mpsc::unbounded_channel();
        let (last_ack, resend) = self.buffer.connect(tx)?;
        let hello = HostFrame::Hello {
            host_id: credentials.host_id,
            host_key: credentials.host_key.clone(),
            protocol: HOST_PROTOCOL_VERSION.into(),
            capabilities,
            last_ack,
        };
        sink.send(text(&hello)?).await.map_err(unavailable)?;

        let writer = async {
            for frame in resend {
                sink.send(text(&frame)?).await.map_err(unavailable)?;
            }
            while let Some(frame) = rx.recv().await {
                sink.send(text(&frame)?).await.map_err(unavailable)?;
            }
            Ok::<_, Error>(())
        };
        let reader = async {
            while let Some(msg) = stream.next().await {
                match msg.map_err(unavailable)? {
                    Message::Text(t) => match parse(&t) {
                        Ok(frame) => self.dispatch(frame),
                        Err(e) => eprintln!("sun-host: bad kernel frame: {e}"),
                    },
                    Message::Close(_) => break,
                    _ => {} // pings are answered by tungstenite
                }
            }
            Ok::<_, Error>(())
        };
        let ended = tokio::select! {
            r = writer => r,
            r = reader => r,
        };
        if let Err(e) = ended {
            eprintln!("sun-host: connection lost: {e}");
        }
        Ok(())
    }
}

/// Reconnect delay for the `attempt`-th retry: `min · 2^attempt` capped at
/// `max`, then a random point in its upper half, so restarted kernels aren't
/// hit by every host at once.
fn backoff(attempt: u32, min: Duration, max: Duration) -> Duration {
    let ceiling = min.saturating_mul(1 << attempt.min(16)).min(max);
    let half = ceiling / 2;
    let random = RandomState::new().hash_one(attempt);
    half + Duration::from_millis(random % (half.as_millis() as u64 + 1))
}

fn text(frame: &HostFrame) -> Result<Message> {
    serde_json::to_string(frame)
        .map(Message::text)
        .map_err(|e| Error::Protocol(format!("encode frame: {e}")))
}

fn parse(text: &str) -> Result<KernelFrame> {
    serde_json::from_str(text).map_err(|e| Error::Protocol(format!("{e}")))
}

fn unavailable(e: tokio_tungstenite::tungstenite::Error) -> Error {
    Error::Unavailable(format!("websocket: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_doubles_from_min_to_max_with_jitter() {
        let (min, max) = (Duration::from_secs(1), Duration::from_secs(60));
        for attempt in 0..40 {
            let ceiling = Duration::from_secs((1u64 << attempt.min(16)).min(60));
            let delay = backoff(attempt, min, max);
            assert!(
                delay >= ceiling / 2 && delay <= ceiling,
                "{attempt}: {delay:?}"
            );
        }
    }
}
