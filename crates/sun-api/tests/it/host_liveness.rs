//! TRD item 15, stage 1: a real `sun_host::Host` against the real kernel
//! through a TCP path the test can cut. Liveness (presence flips on a dead
//! socket and comes back), re-probe end to end (a login after `hello` shows
//! without a restart), "Check again" permissions, and protocol 2.0 hosts
//! next to 2.1 kernels.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use axum::Router;
use axum::http::StatusCode;
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use sun_core::host_protocol::{Capabilities, HostFrame, RuntimeCapability};
use sun_core::model::Role;
use sun_core::{
    BoxStream, EffortCatalog, EffortLevel, Error, HostCtx, HostId, ModelEffort, Readiness, Result,
    RuntimeAdapter, SessionHandle, SessionSpec, Steer, StopMode, SunEvent, Transcript,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio_tungstenite::tungstenite::Message;

use crate::common::{SECRET, kernel};
use crate::workspaces::{call, call_as, member_token};

const MAIN: &str = "00000000000000000000000001";
const ADAPTER: &str = "login-cli";

// ---------- a CLI whose login the test flips ----------

/// A runtime whose login comes and goes, like `claude login` / `logout`
/// run on the host machine. Never starts sessions.
#[derive(Default)]
struct Cli {
    logged_out: AtomicBool,
}

#[async_trait]
impl RuntimeAdapter for Cli {
    fn id(&self) -> &str {
        ADAPTER
    }

    async fn readiness(&self, _: &HostCtx) -> Readiness {
        let logged_in = !self.logged_out.load(Ordering::SeqCst);
        Readiness {
            installed: true,
            version: Some("1.0".into()),
            logged_in,
            detail: (!logged_in).then(|| "Not logged in. Run: login-cli login".into()),
        }
    }

    async fn effort(&self, _: &HostCtx) -> EffortCatalog {
        EffortCatalog {
            effort_options: vec![EffortLevel {
                value: "high".into(),
                label: "High".into(),
                description: None,
            }],
            models: Vec::new(),
        }
    }

    async fn start(&self, _: SessionSpec) -> Result<SessionHandle> {
        Err(Error::Unavailable("no sessions in this test".into()))
    }

    async fn send(&self, _: &SessionHandle, _: Steer) -> Result<()> {
        Err(Error::Unavailable("no sessions in this test".into()))
    }

    async fn stop(&self, _: &SessionHandle, _: StopMode) -> Result<()> {
        Ok(())
    }

    fn events(&self, _: &SessionHandle) -> BoxStream<'static, SunEvent> {
        Box::pin(futures_util::stream::empty())
    }

    async fn export(&self, _: &SessionHandle) -> Result<Transcript> {
        Err(Error::Unavailable("no sessions in this test".into()))
    }

    fn context_window(&self, _: &str) -> Option<u32> {
        None
    }
}

// ---------- a TCP path that can be cut ----------

const PASS: u8 = 0;
/// New connections are closed at once (the host sees an error, backs off).
const REFUSE: u8 = 1;
/// New connections are accepted and then never answered (a load balancer
/// whose backend is gone).
const HOLD: u8 = 2;

/// TCP between host and kernel. [`Path::cut`] freezes every open connection
/// half-open: nothing more crosses and neither end sees a close (a NAT that
/// forgot the flow, a host that lost power, a dropped VPN).
struct NetPath {
    url: String,
    mode: Arc<AtomicU8>,
    open: Arc<Mutex<Vec<Arc<AtomicBool>>>>,
}

impl NetPath {
    async fn to(kernel: SocketAddr) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("ws://{}/host", listener.local_addr().unwrap());
        let mode = Arc::new(AtomicU8::new(PASS));
        let open: Arc<Mutex<Vec<Arc<AtomicBool>>>> = Arc::default();
        let (accept_mode, accept_open) = (mode.clone(), open.clone());
        tokio::spawn(async move {
            loop {
                let Ok((inbound, _)) = listener.accept().await else {
                    return;
                };
                let cut = Arc::new(AtomicBool::new(false));
                match accept_mode.load(Ordering::SeqCst) {
                    REFUSE => continue, // dropped: closed at once
                    HOLD => cut.store(true, Ordering::SeqCst),
                    _ => {}
                }
                let Ok(upstream) = TcpStream::connect(kernel).await else {
                    continue;
                };
                accept_open.lock().unwrap().push(cut.clone());
                let (in_read, in_write) = inbound.into_split();
                let (up_read, up_write) = upstream.into_split();
                tokio::spawn(pipe(in_read, up_write, cut.clone()));
                tokio::spawn(pipe(up_read, in_write, cut));
            }
        });
        Self { url, mode, open }
    }

    /// Freezes every open connection and refuses new ones.
    fn cut(&self) {
        self.mode.store(REFUSE, Ordering::SeqCst);
        for cut in self.open.lock().unwrap().iter() {
            cut.store(true, Ordering::SeqCst);
        }
    }

    /// Freezes every open connection and swallows new ones.
    fn blackhole(&self) {
        self.cut();
        self.mode.store(HOLD, Ordering::SeqCst);
    }

    /// Connections opened through the path so far (held ones too).
    fn connections(&self) -> usize {
        self.open.lock().unwrap().len()
    }

    /// New connections go through again (frozen ones stay frozen).
    fn restore(&self) {
        self.mode.store(PASS, Ordering::SeqCst);
    }
}

/// Copies `from` → `to` until either closes; once `cut`, holds both open
/// forever and forwards nothing, not even a close.
async fn pipe(
    mut from: tokio::net::tcp::OwnedReadHalf,
    mut to: tokio::net::tcp::OwnedWriteHalf,
    cut: Arc<AtomicBool>,
) {
    let mut buf = vec![0; 16 * 1024];
    loop {
        let read = from.read(&mut buf).await;
        if cut.load(Ordering::SeqCst) {
            std::future::pending::<()>().await;
        }
        match read {
            Ok(0) | Err(_) => return,
            Ok(n) => {
                if to.write_all(&buf[..n]).await.is_err() {
                    return;
                }
            }
        }
    }
}

// ---------- helpers ----------

/// Serves `app` on loopback; its address.
async fn serve(app: &Router) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(axum::serve(listener, app.clone()).into_future());
    addr
}

fn host_key(host: HostId) -> String {
    Sha256::digest(format!("{SECRET}:{host}").as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// Starts a real host for `host` dialling `url` with `config`.
fn start_host(
    host: HostId,
    url: &str,
    config: sun_host::HostConfig,
    cli: Arc<Cli>,
) -> tokio::task::JoinHandle<()> {
    let running = sun_host::Host::open(config, vec![cli]).unwrap();
    tokio::spawn(running.run(sun_host::Credentials {
        kernel_url: url.to_owned(),
        host_id: host,
        host_key: host_key(host),
    }))
}

/// `online` / `offline` of `host` in `GET /hosts`.
async fn presence(app: &Router, host: HostId) -> Value {
    let (_, hosts) = call(app, "GET", "/api/v1/hosts", None).await;
    hosts
        .as_array()
        .unwrap()
        .iter()
        .find(|h| h["id"] == json!(host))
        .map_or(Value::Null, |h| h["status"].clone())
}

/// The status of `adapter` in `GET /runtimes` (`ready` / `offline`).
async fn runtime_status(app: &Router, adapter: &str) -> Value {
    let (_, runtimes) = call(app, "GET", "/api/v1/runtimes", None).await;
    runtimes
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["adapter"] == adapter)
        .map_or(Value::Null, |r| r["status"].clone())
}

/// Polls `host`'s presence every 50 ms until it reads `want` or `within`
/// passes; how long it took, or `None`.
async fn presence_becomes(
    app: &Router,
    host: HostId,
    want: &str,
    within: Duration,
) -> Option<Duration> {
    let started = Instant::now();
    while started.elapsed() < within {
        if presence(app, host).await == want {
            return Some(started.elapsed());
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    None
}

fn quick_host(dir: &std::path::Path) -> sun_host::HostConfig {
    let mut config = sun_host::HostConfig::new(dir);
    config.ping_every = Duration::from_millis(50);
    config.liveness = Duration::from_millis(300);
    config.backoff_min = Duration::from_millis(20);
    config.backoff_max = Duration::from_millis(100);
    config
}

// ---------- case 1: liveness ----------

/// Production timings (kernel and host both ping every 10 s and drop after
/// 30 s of silence). The TCP path dies without a close: the kernel shows
/// the host offline well inside 60 s, its runtime stops counting as ready,
/// and once the path is back the host redials and is online and ready again.
#[tokio::test(flavor = "multi_thread")]
async fn a_cut_path_shows_offline_within_60s_and_recovers_with_default_timings() {
    let k = kernel().await;
    let path = NetPath::to(serve(&k.app).await).await;
    let dir = tempfile::tempdir().unwrap();
    let cli = Arc::new(Cli::default());
    let _host = start_host(
        k.host,
        &path.url,
        sun_host::HostConfig::new(dir.path()),
        cli,
    );
    assert!(
        presence_becomes(&k.app, k.host, "online", Duration::from_secs(10))
            .await
            .is_some()
    );
    assert_eq!(runtime_status(&k.app, ADAPTER).await, "ready");

    path.cut();
    let offline = presence_becomes(&k.app, k.host, "offline", Duration::from_secs(60))
        .await
        .expect("still online 60 s after the path died");
    eprintln!("case 1: offline {offline:?} after the cut");
    assert!(
        offline >= Duration::from_secs(15),
        "offline after {offline:?}: not the liveness timeout"
    );
    assert_eq!(
        runtime_status(&k.app, ADAPTER).await,
        "offline",
        "an unreachable host's runtime is not ready"
    );

    path.restore();
    let back = presence_becomes(&k.app, k.host, "online", Duration::from_secs(90))
        .await
        .expect("never came back after the path was restored");
    eprintln!("case 1: online again {back:?} after the restore");
    assert_eq!(runtime_status(&k.app, ADAPTER).await, "ready");
}

/// `kill -9` of the host: the OS closes its socket without a WebSocket
/// close. Offline at once (no 30 s wait), and a restarted host is back.
#[tokio::test(flavor = "multi_thread")]
async fn a_killed_host_goes_offline_and_a_restarted_one_comes_back() {
    let k = kernel().await;
    let addr = serve(&k.app).await;
    let url = format!("ws://{addr}/host");
    let dir = tempfile::tempdir().unwrap();
    let cli = Arc::new(Cli::default());
    let host = start_host(k.host, &url, quick_host(dir.path()), cli.clone());
    assert!(
        presence_becomes(&k.app, k.host, "online", Duration::from_secs(10))
            .await
            .is_some()
    );

    host.abort();
    let offline = presence_becomes(&k.app, k.host, "offline", Duration::from_secs(5))
        .await
        .expect("a closed socket still shows online");
    eprintln!("case 1: offline {offline:?} after the kill");
    assert_eq!(runtime_status(&k.app, ADAPTER).await, "offline");

    let _restarted = start_host(k.host, &url, quick_host(dir.path()), cli);
    assert!(
        presence_becomes(&k.app, k.host, "online", Duration::from_secs(10))
            .await
            .is_some()
    );
    assert_eq!(runtime_status(&k.app, ADAPTER).await, "ready");
}

/// The path dies while the host is already redialling, and the kernel's
/// side then accepts TCP but never answers the WebSocket handshake (a load
/// balancer whose backend is gone). Once the path is back, the host must
/// still find its way back: a dial that never completes is dead too.
#[tokio::test(flavor = "multi_thread")]
async fn a_host_whose_redial_is_swallowed_still_comes_back() {
    let k = kernel().await;
    let app = sun_api::router(
        k.api
            .clone()
            .with_host_liveness(Duration::from_millis(50), Duration::from_millis(300)),
    );
    let path = NetPath::to(serve(&app).await).await;
    let dir = tempfile::tempdir().unwrap();
    let _host = start_host(
        k.host,
        &path.url,
        quick_host(dir.path()),
        Arc::new(Cli::default()),
    );
    assert!(
        presence_becomes(&app, k.host, "online", Duration::from_secs(10))
            .await
            .is_some()
    );

    let before = path.connections();
    path.blackhole();
    assert!(
        presence_becomes(&app, k.host, "offline", Duration::from_secs(5))
            .await
            .is_some()
    );
    // The host noticed and dialled again, into the hole.
    let started = Instant::now();
    while path.connections() == before {
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "never redialled"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    path.restore();
    let back = presence_becomes(&app, k.host, "online", Duration::from_secs(90)).await;
    assert!(
        back.is_some(),
        "the host hung on a handshake that never answers and never redialled"
    );
}

// ---------- case 2: re-probe end to end ----------

/// `login-cli login` on the host machine after `hello`: "Check again"
/// shows it ready in the very next `GET /runtimes`, without a restart; a
/// logout afterwards shows the same way.
#[tokio::test(flavor = "multi_thread")]
async fn check_again_shows_a_login_after_hello_without_a_restart() {
    let k = kernel().await;
    let url = format!("ws://{}/host", serve(&k.app).await);
    let dir = tempfile::tempdir().unwrap();
    let cli = Arc::new(Cli::default());
    cli.logged_out.store(true, Ordering::SeqCst);
    let _host = start_host(k.host, &url, quick_host(dir.path()), cli.clone());
    assert!(
        presence_becomes(&k.app, k.host, "online", Duration::from_secs(10))
            .await
            .is_some()
    );
    assert_eq!(
        runtime_status(&k.app, ADAPTER).await,
        "offline",
        "needs login"
    );

    let check = format!("/api/v1/workspaces/{MAIN}/hosts/{}/reprobe", k.host);
    // Nothing changed: still 204, still not ready.
    assert_eq!(
        call(&k.app, "POST", &check, None).await.0,
        StatusCode::NO_CONTENT
    );
    assert_eq!(runtime_status(&k.app, ADAPTER).await, "offline");

    cli.logged_out.store(false, Ordering::SeqCst);
    let (status, body) = call(&k.app, "POST", &check, None).await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");
    assert_eq!(runtime_status(&k.app, ADAPTER).await, "ready");

    cli.logged_out.store(true, Ordering::SeqCst);
    assert_eq!(
        call(&k.app, "POST", &check, None).await.0,
        StatusCode::NO_CONTENT
    );
    assert_eq!(runtime_status(&k.app, ADAPTER).await, "offline");
}

/// Nobody presses "Check again": the periodic re-probe (shortened from
/// 5 min) carries the login to the kernel on its own.
#[tokio::test(flavor = "multi_thread")]
async fn the_periodic_reprobe_alone_shows_a_login_after_hello() {
    let k = kernel().await;
    let url = format!("ws://{}/host", serve(&k.app).await);
    let dir = tempfile::tempdir().unwrap();
    let mut config = quick_host(dir.path());
    config.reprobe_every = Duration::from_millis(100);
    let cli = Arc::new(Cli::default());
    cli.logged_out.store(true, Ordering::SeqCst);
    let _host = start_host(k.host, &url, config, cli.clone());
    assert!(
        presence_becomes(&k.app, k.host, "online", Duration::from_secs(10))
            .await
            .is_some()
    );
    assert_eq!(runtime_status(&k.app, ADAPTER).await, "offline");

    cli.logged_out.store(false, Ordering::SeqCst);
    let started = Instant::now();
    let mut status = Value::Null;
    while started.elapsed() < Duration::from_secs(10) {
        status = runtime_status(&k.app, ADAPTER).await;
        if status == "ready" {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(status, "ready", "after {:?}", started.elapsed());
}

/// "Check again" is a `manage_runtimes` action: admins only, and a host of
/// a workspace the caller can't see is a 404.
#[tokio::test(flavor = "multi_thread")]
async fn check_again_needs_manage_runtimes_in_that_workspace() {
    let k = kernel().await;
    let check = format!("/api/v1/workspaces/{MAIN}/hosts/{}/reprobe", k.host);
    for role in [Role::Member, Role::Viewer] {
        let token = member_token(&k.store, &format!("{role:?}").to_lowercase(), role).await;
        let (status, body) = call_as(&k.app, &token, "POST", &check, None).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{role:?}: {body}");
    }
    let admin = member_token(&k.store, "admin", Role::Admin).await;
    // Allowed, but the host isn't connected: 503, not a hang.
    let (status, body) = call_as(&k.app, &admin, "POST", &check, None).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{body}");

    let other = format!(
        "/api/v1/workspaces/{MAIN}/hosts/{}/reprobe",
        HostId::generate()
    );
    assert_eq!(
        call_as(&k.app, &admin, "POST", &other, None).await.0,
        StatusCode::NOT_FOUND
    );
    let bad = format!("/api/v1/workspaces/{MAIN}/hosts/not-an-id/reprobe");
    let (status, _) = call_as(&k.app, &admin, "POST", &bad, None).await;
    assert!(status.is_client_error(), "{status}");
}

// ---------- case 4: protocol 2.0 hosts ----------

/// A 2.0 host never pings and never sends `capabilities`; it only answers
/// the kernel's pings (tungstenite does, while the host reads). It stays
/// online across many liveness windows and its runtime is listed ready.
#[tokio::test(flavor = "multi_thread")]
async fn a_2_0_host_that_never_pings_stays_online_and_listed() {
    let k = kernel().await;
    let app = sun_api::router(
        k.api
            .clone()
            .with_host_liveness(Duration::from_millis(50), Duration::from_millis(300)),
    );
    let url = format!("ws://{}/host", serve(&app).await);
    let (mut socket, _) = tokio_tungstenite::connect_async(&url).await.unwrap();
    // Byte for byte what a 2.0 host sends: no effort fields anywhere.
    let hello = json!({
        "type": "hello", "host_id": k.host, "host_key": host_key(k.host), "protocol": "2.0",
        "capabilities": { "os": "linux", "arch": "x86_64", "cpus": 4, "runtimes": [
            { "adapter": "claude-code", "readiness": { "installed": true, "version": "2.1.291", "logged_in": true, "detail": null } }
        ] },
        "last_ack": {},
    });
    socket.send(Message::text(hello.to_string())).await.unwrap();
    // The 2.0 reader loop: reads everything, answers pings, sends nothing.
    let reader = tokio::spawn(async move { while let Some(Ok(_)) = socket.next().await {} });
    assert!(
        presence_becomes(&app, k.host, "online", Duration::from_secs(10))
            .await
            .is_some()
    );
    let started = Instant::now();
    while started.elapsed() < Duration::from_millis(1500) {
        assert_eq!(
            presence(&app, k.host).await,
            "online",
            "dropped after {:?} with pongs flowing",
            started.elapsed()
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(!reader.is_finished(), "the kernel closed the 2.0 host");
    assert_eq!(runtime_status(&app, "claude-code").await, "ready");
    reader.abort();
}

/// Every field item 1 added deserializes from a report that lacks it.
#[test]
fn every_new_field_has_a_serde_default() {
    let old = json!({ "adapter": "codex",
        "readiness": { "installed": true, "version": "0.1", "logged_in": true, "detail": null } });
    let runtime: RuntimeCapability = serde_json::from_value(old).unwrap();
    assert_eq!(runtime.effort, EffortCatalog::default());

    let model: ModelEffort =
        serde_json::from_value(json!({ "id": "gpt-5", "label": "GPT-5" })).unwrap();
    assert!(model.effort_options.is_empty());
    assert_eq!(model.default_effort, None);

    let level: EffortLevel =
        serde_json::from_value(json!({ "value": "high", "label": "High" })).unwrap();
    assert_eq!(level.description, None);
    // ...and a missing description stays missing on the wire.
    assert_eq!(
        serde_json::to_value(&level).unwrap(),
        json!({ "value": "high", "label": "High" })
    );

    let catalog: EffortCatalog = serde_json::from_value(json!({})).unwrap();
    assert_eq!(catalog, EffortCatalog::default());

    // A full 2.0 `hello` parses as today's frame.
    let hello = json!({ "type": "hello", "host_id": HostId::generate(), "host_key": "k",
        "protocol": "2.0",
        "capabilities": { "os": "linux", "arch": "x86_64", "cpus": 1, "runtimes": [
            { "adapter": "claude-code", "readiness": { "installed": false, "version": null, "logged_in": false, "detail": null } }
        ] }, "last_ack": {} });
    let HostFrame::Hello { capabilities, .. } = serde_json::from_value(hello).unwrap() else {
        panic!("not a hello");
    };
    let Capabilities { runtimes, .. } = capabilities;
    assert_eq!(runtimes[0].effort, EffortCatalog::default());
}
