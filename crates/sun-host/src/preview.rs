//! Design-canvas previews: the repo's dev server, run on the host's
//! loopback for a task's `after` (worktree) or `before` (base branch)
//! checkout, and a plain HTTP/1.0 client the kernel tunnels requests through
//! (`preview.http`). Nothing listens on a public interface.
//!
//! The command is the first line of `.sun-preview` in the task's worktree
//! (later lines list the canvas screens, read by the kernel),
//! run as `sh -c` in the checkout with `PORT`, `HOST=127.0.0.1` and
//! `SUN_PREVIEW_BASE` (the URL prefix the app must be served under).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use sun_core::host_protocol::{
    HttpRequest, HttpResponse, PreviewRequest, PreviewSide, ScreenshotRequest,
};
use sun_core::redact::redact;
use sun_core::{Error, Result, TaskId};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::process::{Child, Command};
use tokio::sync::Mutex;

use crate::process;

/// File in the repo root that declares the dev server command.
pub const COMMAND_FILE: &str = sun_core::host_protocol::PREVIEW_FILE;
/// Largest response body tunnelled back (base64 grows it by a third).
const MAX_BODY: usize = 8 * 1024 * 1024;
/// How long one proxied request may take.
const HTTP_TIMEOUT: Duration = Duration::from_secs(60);
/// Headers that belong to one hop, never forwarded either way.
const HOP_BY_HOP: &[&str] = &[
    "connection",
    "keep-alive",
    "proxy-connection",
    "transfer-encoding",
    "upgrade",
    "te",
    "trailer",
    "host",
    "content-length",
];

/// Headless browsers tried for screenshots, in order; `SUN_BROWSER` wins.
const BROWSERS: &[&str] = &[
    "chromium",
    "chromium-browser",
    "google-chrome",
    "google-chrome-stable",
    "chrome",
    "chrome-headless-shell",
    "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
    "/Applications/Chromium.app/Contents/MacOS/Chromium",
];
/// How long one screenshot may take (load, scripts, paint).
const SCREENSHOT_TIMEOUT: Duration = Duration::from_secs(60);

type Key = (TaskId, PreviewSide);

struct Server {
    port: u16,
    base_path: String,
    child: Child,
}

/// The running dev servers, one per task and side.
// ponytail: servers live until `preview.stop` or the host exits (then only
// `sh` dies, not its group); add an idle reaper if hosts run many previews.
pub struct Previews {
    logs: PathBuf,
    running: Mutex<HashMap<Key, Server>>,
}

impl Previews {
    /// Previews whose server logs go under `logs`.
    pub fn new(logs: PathBuf) -> Self {
        Self {
            logs,
            running: Mutex::new(HashMap::new()),
        }
    }

    /// Starts the dev server for `request` in `dir` (already checked), with
    /// the command from `worktree`'s `.sun-preview`, and waits until its port
    /// accepts connections. A live server with the same base is reused.
    pub async fn start(
        &self,
        task_id: TaskId,
        worktree: &Path,
        dir: &Path,
        request: &PreviewRequest,
    ) -> Result<()> {
        let key = (task_id, request.side);
        if let Some(old) = self.running.lock().await.get_mut(&key)
            && old.base_path == request.base_path
            && matches!(old.child.try_wait(), Ok(None))
        {
            return Ok(());
        }
        let command = std::fs::read_to_string(worktree.join(COMMAND_FILE))
            .ok()
            .and_then(|text| text.lines().next().map(|l| l.trim().to_owned()))
            .filter(|line| !line.is_empty())
            .ok_or_else(|| {
                Error::InvalidInput(format!(
                    "the repo declares no preview: add a `{COMMAND_FILE}` file with the dev server command"
                ))
            })?;
        let port = std::net::TcpListener::bind("127.0.0.1:0")
            .and_then(|l| l.local_addr())
            .map_err(|e| Error::Unavailable(format!("no free port: {e}")))?
            .port();
        std::fs::create_dir_all(&self.logs)
            .map_err(|e| Error::Unavailable(format!("{}: {e}", self.logs.display())))?;
        let log_path = self
            .logs
            .join(format!("preview-{task_id}-{}.log", request.side));
        let log = std::fs::File::create(&log_path)
            .map_err(|e| Error::Unavailable(format!("{}: {e}", log_path.display())))?;
        let err_log = log
            .try_clone()
            .map_err(|e| Error::Unavailable(e.to_string()))?;
        let mut cmd = Command::new("sh");
        cmd.arg("-c")
            .arg(&command)
            .current_dir(dir)
            .env("PORT", port.to_string())
            .env("HOST", "127.0.0.1")
            .env("BROWSER", "none")
            .env("SUN_PREVIEW_BASE", &request.base_path);
        // The repo's own dev server: none of the host's `SUN_*` variables.
        sun_core::scrub_kernel_env(cmd.as_std_mut());
        let mut child = cmd
            .process_group(0)
            .stdin(Stdio::null())
            .stdout(log)
            .stderr(err_log)
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| Error::Unavailable(format!("cannot start the preview: {e}")))?;

        let deadline = tokio::time::Instant::now() + Duration::from_secs(request.timeout_s);
        loop {
            if let Ok(Some(status)) = child.try_wait() {
                return Err(Error::Unavailable(format!(
                    "the preview command exited ({status}):\n{}",
                    tail(&log_path)
                )));
            }
            if TcpStream::connect(("127.0.0.1", port)).await.is_ok() {
                break;
            }
            if tokio::time::Instant::now() >= deadline {
                stop(child).await;
                return Err(Error::Unavailable(format!(
                    "the preview did not listen on $PORT within {}s:\n{}",
                    request.timeout_s,
                    tail(&log_path)
                )));
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
        let server = Server {
            port,
            base_path: request.base_path.clone(),
            child,
        };
        if let Some(old) = self.running.lock().await.insert(key, server) {
            stop(old.child).await;
        }
        Ok(())
    }

    /// Stops both servers of a task. Idempotent.
    pub async fn stop(&self, task_id: TaskId) {
        for side in PreviewSide::ALL {
            let server = self.running.lock().await.remove(&(task_id, *side));
            if let Some(server) = server {
                stop(server.child).await;
            }
        }
    }

    /// Whether the side's dev server is up.
    pub async fn is_running(&self, task_id: TaskId, side: PreviewSide) -> bool {
        self.running
            .lock()
            .await
            .get_mut(&(task_id, side))
            .is_some_and(|s| matches!(s.child.try_wait(), Ok(None)))
    }

    /// A PNG of `request.path` on the task's running server, from a headless
    /// Chrome/Chromium at the requested viewport (device pixel ratio 1).
    /// `Unavailable` when the host has no such browser.
    pub async fn screenshot(
        &self,
        task_id: TaskId,
        request: &ScreenshotRequest,
    ) -> Result<Vec<u8>> {
        let path = &request.path;
        if !path.starts_with('/')
            || path.len() > 2000
            || path.chars().any(|c| c.is_whitespace() || c.is_control())
            || !(100..=4000).contains(&request.width)
            || !(100..=8000).contains(&request.height)
        {
            return Err(Error::InvalidInput(format!(
                "screenshot: bad page `{path}` or size {}x{}",
                request.width, request.height
            )));
        }
        let (port, base) = self
            .running
            .lock()
            .await
            .get_mut(&(task_id, request.side))
            .and_then(|s| {
                matches!(s.child.try_wait(), Ok(None)).then(|| (s.port, s.base_path.clone()))
            })
            .ok_or_else(|| {
                Error::NotFound(format!(
                    "no {} preview is running for task {task_id}",
                    request.side
                ))
            })?;
        let browser = browser().ok_or_else(|| {
            Error::Unavailable(
                "no headless Chrome or Chromium on this host: install one or set SUN_BROWSER"
                    .into(),
            )
        })?;
        let url = format!(
            "http://127.0.0.1:{port}{}{path}",
            base.trim_end_matches('/')
        );
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos());
        let dir = self
            .logs
            .join(format!("shot-{task_id}-{}-{nanos}", request.side));
        let out = dir.join("shot.png");
        std::fs::create_dir_all(&dir)
            .map_err(|e| Error::Unavailable(format!("{}: {e}", dir.display())))?;
        let mut child = Command::new(&browser)
            .args([
                "--headless=new",
                "--disable-gpu",
                "--hide-scrollbars",
                "--no-first-run",
                "--no-default-browser-check",
                "--mute-audio",
                "--force-device-scale-factor=1",
                "--virtual-time-budget=5000",
            ])
            .arg(format!("--user-data-dir={}", dir.join("profile").display()))
            .arg(format!(
                "--window-size={},{}",
                request.width, request.height
            ))
            .arg(format!("--screenshot={}", out.display()))
            .arg(&url)
            .process_group(0)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| Error::Unavailable(format!("{}: {e}", browser.display())))?;
        let waited = tokio::time::timeout(SCREENSHOT_TIMEOUT, child.wait()).await;
        if waited.is_err() {
            stop(child).await;
        }
        let png = std::fs::read(&out);
        let _ = std::fs::remove_dir_all(&dir);
        match (waited, png) {
            (Ok(_), Ok(png)) if !png.is_empty() => Ok(png),
            (Err(_), _) => Err(Error::Unavailable(format!(
                "the browser took over {}s on {path}",
                SCREENSHOT_TIMEOUT.as_secs()
            ))),
            (_, _) => Err(Error::Unavailable(format!(
                "the browser made no screenshot of {path}"
            ))),
        }
    }

    /// Sends `request` to the task's running server and returns its answer.
    pub async fn http(
        &self,
        task_id: TaskId,
        side: PreviewSide,
        request: &HttpRequest,
    ) -> Result<HttpResponse> {
        let port = self
            .running
            .lock()
            .await
            .get_mut(&(task_id, side))
            .and_then(|s| matches!(s.child.try_wait(), Ok(None)).then_some(s.port))
            .ok_or_else(|| {
                Error::NotFound(format!("no {side} preview is running for task {task_id}"))
            })?;
        let raw = encode(request, port)?;
        let exchange = async {
            let mut stream = TcpStream::connect(("127.0.0.1", port)).await?;
            stream.write_all(&raw).await?;
            let mut answer = Vec::new();
            stream
                .take((MAX_BODY + 64 * 1024) as u64 + 1)
                .read_to_end(&mut answer)
                .await?;
            std::io::Result::Ok(answer)
        };
        let answer = tokio::time::timeout(HTTP_TIMEOUT, exchange)
            .await
            .map_err(|_| Error::Unavailable("the preview did not answer in time".into()))?
            .map_err(|e| Error::Unavailable(format!("preview: {e}")))?;
        decode(&answer)
    }
}

async fn stop(child: Child) {
    if let Some(pgid) = child.id() {
        process::kill_group(pgid).await;
    }
    drop(child);
}

/// The headless browser to screenshot with: `SUN_BROWSER`, else the first
/// of [`BROWSERS`] found (bare names on `PATH`).
fn browser() -> Option<PathBuf> {
    if let Some(set) = std::env::var_os("SUN_BROWSER").filter(|v| !v.is_empty()) {
        return Some(PathBuf::from(set));
    }
    let path = std::env::var_os("PATH").unwrap_or_default();
    BROWSERS.iter().find_map(|name| {
        let name = Path::new(name);
        if name.is_absolute() {
            return name.is_file().then(|| name.to_path_buf());
        }
        std::env::split_paths(&path)
            .map(|dir| dir.join(name))
            .find(|p| p.is_file())
    })
}

/// The last 4 KiB of a server log, redacted.
fn tail(path: &Path) -> String {
    let bytes = std::fs::read(path).unwrap_or_default();
    let start = bytes.len().saturating_sub(4096);
    redact(&String::from_utf8_lossy(&bytes[start..]), &[])
}

fn is_token(text: &str) -> bool {
    !text.is_empty()
        && text
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&b))
}

/// The request as HTTP/1.0 bytes (so the server closes the connection and
/// sends no chunked body). Rejects anything that could split the request.
fn encode(request: &HttpRequest, port: u16) -> Result<Vec<u8>> {
    let bad = |what: &str| Err(Error::InvalidInput(format!("preview request: bad {what}")));
    if request.method.len() > 16 || !is_token(&request.method) {
        return bad("method");
    }
    if !request.path.starts_with('/')
        || request.path.len() > 8192
        || request.path.bytes().any(|b| b <= b' ' || b == 0x7f)
    {
        return bad("path");
    }
    let mut head = format!(
        "{} {} HTTP/1.0\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n",
        request.method, request.path
    );
    for (name, value) in &request.headers {
        if !is_token(name) || value.bytes().any(|b| b == b'\r' || b == b'\n' || b == 0) {
            return bad("header");
        }
        if !HOP_BY_HOP.contains(&name.to_ascii_lowercase().as_str()) {
            head.push_str(&format!("{name}: {value}\r\n"));
        }
    }
    if !request.body.is_empty() {
        head.push_str(&format!("Content-Length: {}\r\n", request.body.len()));
    }
    head.push_str("\r\n");
    let mut raw = head.into_bytes();
    raw.extend_from_slice(&request.body);
    Ok(raw)
}

/// Parses a whole HTTP/1.x response read until the server closed.
fn decode(raw: &[u8]) -> Result<HttpResponse> {
    let bad = |what: &str| Error::Protocol(format!("preview answered {what}"));
    let split = raw
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .ok_or_else(|| bad("no HTTP headers"))?;
    let head = String::from_utf8_lossy(&raw[..split]);
    let mut lines = head.split("\r\n");
    let status = lines
        .next()
        .filter(|l| l.starts_with("HTTP/1."))
        .and_then(|l| l.split(' ').nth(1))
        .and_then(|code| code.parse::<u16>().ok())
        .ok_or_else(|| bad("a bad status line"))?;
    let mut headers = Vec::new();
    for line in lines {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        let lower = name.trim().to_ascii_lowercase();
        if lower == "transfer-encoding" {
            // ponytail: HTTP/1.0 servers never chunk; decode chunks if one does.
            return Err(bad("a chunked body to an HTTP/1.0 request"));
        }
        if !HOP_BY_HOP.contains(&lower.as_str()) {
            headers.push((name.trim().to_owned(), value.trim().to_owned()));
        }
    }
    let body = raw[split + 4..].to_vec();
    if body.len() > MAX_BODY {
        return Err(Error::Unavailable("the preview answered over 8 MiB".into()));
    }
    Ok(HttpResponse {
        status,
        headers,
        body,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requests_cannot_be_split_and_responses_parse() {
        let request = |method: &str, path: &str, header: (&str, &str)| HttpRequest {
            method: method.into(),
            path: path.into(),
            headers: vec![(header.0.into(), header.1.into())],
            body: b"hi".to_vec(),
        };
        let raw = encode(&request("POST", "/p/x?a=1", ("Accept", "*/*")), 9).unwrap();
        assert_eq!(
            String::from_utf8(raw).unwrap(),
            "POST /p/x?a=1 HTTP/1.0\r\nHost: 127.0.0.1:9\r\nConnection: close\r\n\
             Accept: */*\r\nContent-Length: 2\r\n\r\nhi"
        );
        let hop = encode(&request("GET", "/", ("Connection", "upgrade")), 9).unwrap();
        assert!(!String::from_utf8(hop).unwrap().contains("upgrade"));
        for bad in [
            request("GET /x HTTP/1.1\r\n", "/", ("a", "b")),
            request("GET", "/ HTTP/1.1\r\nX: y", ("a", "b")),
            request("GET", "http://evil/", ("a", "b")),
            request("GET", "/", ("a\r\nb", "c")),
            request("GET", "/", ("a", "b\r\nEvil: 1")),
        ] {
            assert!(encode(&bad, 9).is_err(), "{bad:?}");
        }

        let response =
            decode(b"HTTP/1.0 404 Not Found\r\nContent-Type: text/html\r\nConnection: close\r\nContent-Length: 2\r\n\r\nno")
                .unwrap();
        assert_eq!(response.status, 404);
        assert_eq!(
            response.headers,
            [("Content-Type".to_owned(), "text/html".to_owned())]
        );
        assert_eq!(response.body, b"no");
        assert!(decode(b"garbage").is_err());
        assert!(
            decode(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n2\r\nhi\r\n0\r\n\r\n")
                .is_err()
        );
    }
}
