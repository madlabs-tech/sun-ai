//! Terminals end to end: the real `sun-host` behind the kernel, a shell on
//! a PTY in the editor worktree over the browser socket, its exit code,
//! resize, reattach within the grace, the session killed after it, the
//! per-member cap, and the `terminal` capability checked on every connect.
#![allow(clippy::unwrap_used, clippy::expect_used)] // test helpers

use std::time::{Duration, Instant};

use axum::http::StatusCode;
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use sun_core::WorkspaceId;
use sun_core::model::Role;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::{self, Message};

use crate::common::{self, SECRET, kernel};
use crate::editor_grants::member_id;
use crate::workspaces::{call, call_as, member_token};

pub(crate) type Socket =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

/// A kernel on TCP with a real host and a one-file repo; the base path of
/// the repo's routes and the kernel's address.
pub(crate) struct World {
    pub(crate) k: common::Kernel,
    pub(crate) addr: std::net::SocketAddr,
    pub(crate) base: String,
    pub(crate) _dir: tempfile::TempDir,
}

fn git(cwd: &std::path::Path, args: &[&str]) {
    let ok = std::process::Command::new("git")
        .args(["-c", "user.email=t@t", "-c", "user.name=t"])
        .args(args)
        .current_dir(cwd)
        .status()
        .unwrap();
    assert!(ok.success(), "git {args:?}");
}

pub(crate) async fn world() -> World {
    world_with(kernel().await, Duration::from_secs(1)).await
}

/// [`world`] on `k`, a closed browser's terminal kept for `grace`.
pub(crate) async fn world_with(k: common::Kernel, grace: Duration) -> World {
    let app = sun_api::router(k.api.clone().with_terminal_grace(grace));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(axum::serve(listener, app).into_future());
    let dir = tempfile::tempdir().unwrap();
    let origin = dir.path().join("origin");
    std::fs::create_dir_all(&origin).unwrap();
    std::fs::write(origin.join("README.md"), "hi\n").unwrap();
    git(&origin, &["init", "-q", "-b", "main"]);
    git(&origin, &["add", "."]);
    git(&origin, &["commit", "-q", "-m", "base"]);
    let host =
        sun_host::Host::open(sun_host::HostConfig::new(dir.path().join("host")), vec![]).unwrap();
    let key: String = Sha256::digest(format!("{SECRET}:{}", k.host).as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    tokio::spawn(host.run(sun_host::Credentials {
        kernel_url: format!("ws://{addr}/host"),
        host_id: k.host,
        host_key: key,
    }));
    let repo = common::local_repo(&k, WorkspaceId::MAIN, origin.to_str().unwrap()).await;
    let base = format!("/api/v1/workspaces/{}/repos/{repo}", WorkspaceId::MAIN);
    World {
        k,
        addr,
        base,
        _dir: dir,
    }
}

impl World {
    /// Opens `token`'s editor worktree (waiting for the host to dial in).
    pub(crate) async fn open_editor(&self, token: &str) {
        let open = format!("{}/editor/open?host_id={}", self.base, self.k.host);
        for _ in 0..500 {
            let (status, body) = call_as(&self.k.app, token, "POST", &open, None).await;
            if status == StatusCode::OK {
                return;
            }
            assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{body}");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("the host never came");
    }

    /// The terminal socket as `token`, with `query` after `host_id`.
    pub(crate) async fn connect(
        &self,
        token: &str,
        query: &str,
    ) -> Result<Socket, tungstenite::Error> {
        let url = format!(
            "ws://{}{}/terminal?host_id={}{query}",
            self.addr, self.base, self.k.host
        );
        let mut request = url.into_client_request().unwrap();
        let bearer = format!("Bearer {token}").parse().unwrap();
        request.headers_mut().insert("authorization", bearer);
        tokio_tungstenite::connect_async(request)
            .await
            .map(|(ws, _)| ws)
    }
}

/// The HTTP status a refused upgrade answered.
pub(crate) fn refused(result: Result<Socket, tungstenite::Error>) -> StatusCode {
    match result {
        Err(tungstenite::Error::Http(response)) => {
            StatusCode::from_u16(response.status().as_u16()).unwrap()
        }
        Err(other) => panic!("{other}"),
        Ok(_) => panic!("the upgrade went through"),
    }
}

/// What a terminal socket said: bytes so far and its control messages.
#[derive(Default)]
pub(crate) struct Screen {
    pub(crate) output: String,
    pub(crate) controls: Vec<Value>,
}

/// Reads until `done(screen)` holds or the socket ends; panics after 20 s.
pub(crate) async fn read_until(
    ws: &mut Socket,
    screen: &mut Screen,
    done: impl Fn(&Screen) -> bool,
) {
    let deadline = Instant::now() + Duration::from_secs(20);
    while !done(screen) {
        let left = deadline.saturating_duration_since(Instant::now());
        let message = tokio::time::timeout(left, ws.next()).await;
        match message.unwrap_or_else(|_| panic!("timed out; saw {:?}", screen.output)) {
            Some(Ok(Message::Binary(bytes))) => {
                screen.output.push_str(&String::from_utf8_lossy(&bytes));
            }
            Some(Ok(Message::Text(text))) => {
                screen.controls.push(serde_json::from_str(&text).unwrap());
            }
            Some(Ok(_)) => {}
            None | Some(Err(_)) => return,
        }
    }
}

/// Reads up to `open`; the terminal's id.
pub(crate) async fn opened(ws: &mut Socket) -> String {
    let mut screen = Screen::default();
    read_until(ws, &mut screen, |s| !s.controls.is_empty()).await;
    assert_eq!(screen.controls[0]["kind"], "open", "{:?}", screen.controls);
    screen.controls[0]["pty_id"].as_str().unwrap().to_owned()
}

pub(crate) async fn type_line(ws: &mut Socket, line: &str) {
    let bytes = format!("{line}\r").into_bytes();
    ws.send(Message::binary(bytes)).await.unwrap();
}

/// `kill -0 pid` succeeds: the process exists, a zombie included.
pub(crate) fn exists(pid: &str) -> bool {
    std::process::Command::new("kill")
        .args(["-0", pid])
        .stderr(std::process::Stdio::null())
        .status()
        .unwrap()
        .success()
}

/// `name=<digits>` in `text`, after the typed line that has `name=$…`.
pub(crate) fn number_after(text: &str, name: &str) -> Option<String> {
    text.match_indices(&format!("{name}="))
        .map(|(at, found)| &text[at + found.len()..])
        .map(|rest| {
            rest.chars()
                .take_while(char::is_ascii_digit)
                .collect::<String>()
        })
        .find(|digits| !digits.is_empty())
}

#[tokio::test(flavor = "multi_thread")]
async fn a_terminal_runs_resizes_reattaches_and_its_session_dies_with_it() {
    let w = world().await;
    let owner = member_token(&w.k.store, "olive", Role::Owner).await;
    w.open_editor(&owner).await;

    // `echo hi && exit 3`: the output, then exit code 3, then close.
    let mut ws = w.connect(&owner, "&cols=80&rows=24").await.unwrap();
    opened(&mut ws).await;
    type_line(&mut ws, "echo hi && exit 3").await;
    let mut screen = Screen::default();
    read_until(&mut ws, &mut screen, |s| {
        s.controls.iter().any(|c| c["kind"] == "exit")
    })
    .await;
    // The typed line echoes as `hi &&`; only the output ends a line.
    assert!(screen.output.contains("hi\r\n"), "{:?}", screen.output);
    assert_eq!(screen.controls, [json!({"kind": "exit", "code": 3})]);
    assert!(
        matches!(ws.next().await, None | Some(Ok(Message::Close(_)))),
        "closed after exit"
    );

    // Resize, in the editor worktree, `stty` sees it.
    let mut ws = w.connect(&owner, "&cols=80&rows=24").await.unwrap();
    let pty_id = opened(&mut ws).await;
    let resize = json!({"kind": "resize", "cols": 100, "rows": 30}).to_string();
    ws.send(Message::text(resize)).await.unwrap();
    type_line(&mut ws, "stty size; ls").await;
    let mut screen = Screen::default();
    read_until(&mut ws, &mut screen, |s| s.output.contains("README.md")).await;
    assert!(screen.output.contains("30 100"), "{:?}", screen.output);

    // A job in a process group of its own; the browser leaves and comes
    // back within the grace: same shell.
    type_line(&mut ws, "sleep 1000 & echo shell=$$ job=$!").await;
    let mut screen = Screen::default();
    read_until(&mut ws, &mut screen, |s| {
        number_after(&s.output, "job").is_some()
    })
    .await;
    let shell = number_after(&screen.output, "shell").unwrap();
    let job = number_after(&screen.output, "job").unwrap();
    assert!(exists(&shell) && exists(&job));
    drop(ws);
    let mut ws = w
        .connect(&owner, &format!("&pty_id={pty_id}"))
        .await
        .unwrap();
    assert_eq!(opened(&mut ws).await, pty_id);
    type_line(&mut ws, "echo again=$$").await;
    let mut screen = Screen::default();
    read_until(&mut ws, &mut screen, |s| {
        number_after(&s.output, "again").is_some()
    })
    .await;
    assert_eq!(number_after(&screen.output, "again").unwrap(), shell);

    // Gone past the grace: the shell and its job are killed and reaped,
    // and the terminal can't be reattached.
    drop(ws);
    let deadline = Instant::now() + Duration::from_secs(15);
    while (exists(&shell) || exists(&job)) && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(!exists(&shell), "shell {shell} outlived its terminal");
    assert!(!exists(&job), "job {job} outlived its terminal");
    let again = w.connect(&owner, &format!("&pty_id={pty_id}")).await;
    assert_eq!(refused(again), StatusCode::NOT_FOUND);

    // Five per member per host; the sixth says why.
    let mut open = Vec::new();
    for _ in 0..5 {
        let mut ws = w.connect(&owner, "").await.unwrap();
        opened(&mut ws).await;
        open.push(ws);
    }
    let mut sixth = w.connect(&owner, "").await.unwrap();
    let mut screen = Screen::default();
    read_until(&mut sixth, &mut screen, |s| !s.controls.is_empty()).await;
    assert_eq!(screen.controls[0]["kind"], "error");
    assert!(
        screen.controls[0]["message"]
            .as_str()
            .unwrap()
            .contains("at most 5"),
        "{:?}",
        screen.controls
    );

    // A bad size is refused before anything opens.
    let bad = w.connect(&owner, "&cols=0&rows=24").await;
    assert_eq!(refused(bad), StatusCode::UNPROCESSABLE_ENTITY);

    // Audited: open and close, with the duration, and nothing typed.
    let (status, audit) = call(&w.k.app, "GET", "/api/v1/audit", None).await;
    assert_eq!(status, StatusCode::OK, "{audit}");
    let terminal: Vec<String> = audit
        .as_array()
        .unwrap()
        .iter()
        .filter(|e| e["action"] == "terminal")
        .map(|e| e["detail"].as_str().unwrap().to_owned())
        .collect();
    assert!(
        terminal.iter().any(|d| d == "terminal.open"),
        "{terminal:?}"
    );
    assert!(
        terminal
            .iter()
            .any(|d| d.starts_with("terminal.close after ")),
        "{terminal:?}"
    );
    assert!(
        !audit.to_string().contains("echo"),
        "keystrokes in the audit"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn the_terminal_capability_is_checked_on_every_connect() {
    let w = world().await;
    let member = member_token(&w.k.store, "mia", Role::Member).await;
    let mia = member_id(&w.k.store, "mia").await;
    let grants = format!("/api/v1/workspaces/{}/editor/grants", WorkspaceId::MAIN);
    let grant = |capability: &str| {
        Some(json!({"subject_type": "member", "subject_id": mia, "capability": capability}))
    };
    let app = &w.k.app;

    // Write is not terminal: refused before any PTY is asked for.
    let (status, _) = call_as(app, common::TOKEN, "POST", &grants, grant("editor.write")).await;
    assert_eq!(status, StatusCode::CREATED);
    w.open_editor(&member).await;
    assert_eq!(refused(w.connect(&member, "").await), StatusCode::FORBIDDEN);
    // Nor does the deployment token, which is no member.
    let token = w.connect(common::TOKEN, "").await;
    assert_eq!(refused(token), StatusCode::FORBIDDEN);

    let (status, made) = call_as(app, common::TOKEN, "POST", &grants, grant("terminal")).await;
    assert_eq!(status, StatusCode::CREATED, "{made}");
    let mut ws = w.connect(&member, "").await.unwrap();
    let pty_id = opened(&mut ws).await;

    // Revoked: the next connect, a reattach included, is refused.
    let revoke = format!("{grants}/{}", made["id"].as_str().unwrap());
    let (status, _) = call_as(app, common::TOKEN, "DELETE", &revoke, None).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    drop(ws);
    let again = w.connect(&member, &format!("&pty_id={pty_id}")).await;
    assert_eq!(refused(again), StatusCode::FORBIDDEN);
}

/// Set in the child process [`a_terminal_cannot_read_the_kernels_environ`] starts.
const ENVIRON_CHILD: &str = "TERMINAL_ENVIRON_CHILD";

/// The child's half: a kernel with its in-process host (as `sun serve
/// --with-host`), `SUN_MASTER_KEY` in its env; the shell's parent's
/// `/proc/$PPID/environ` (the kernel's) typed in a terminal, printed.
#[cfg(target_os = "linux")]
#[tokio::test(flavor = "multi_thread")]
async fn kernel_environ_child() {
    if std::env::var_os(ENVIRON_CHILD).is_none() {
        return;
    }
    let w = world().await;
    let owner = member_token(&w.k.store, "own", Role::Owner).await;
    w.open_editor(&owner).await;
    let mut ws = w.connect(&owner, "").await.unwrap();
    opened(&mut ws).await;
    type_line(
        &mut ws,
        "tr '\\0' '\\n' < /proc/$PPID/environ; echo END-$((40+2))",
    )
    .await;
    let mut screen = Screen::default();
    read_until(&mut ws, &mut screen, |s| s.output.contains("END-42")).await;
    println!("PTY<<<{}>>>PTY", screen.output);
}

/// BLO-201: the kernel is non-dumpable (`PR_SET_DUMPABLE` 0), so a shell on
/// its own host, the same uid, can't read the kernel's `/proc/<pid>/environ`.
#[cfg(target_os = "linux")]
#[test]
fn a_terminal_cannot_read_the_kernels_environ() {
    const MASTER_KEY: &str = "7e57ab1e0000000000000000000000000000000000000000000000000000beef";
    let exe = std::env::current_exe().unwrap();
    let out = std::process::Command::new(exe)
        .args([
            "--exact",
            "terminal::kernel_environ_child",
            "--nocapture",
            "--test-threads=1",
        ])
        .env(ENVIRON_CHILD, "1")
        .env("SUN_MASTER_KEY", MASTER_KEY)
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    assert!(
        out.status.success(),
        "{stdout}{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let (_, rest) = stdout.split_once("PTY<<<").expect("the child ran");
    let (seen, _) = rest.split_once(">>>PTY").unwrap();
    assert!(seen.to_lowercase().contains("permission denied"), "{seen}");
    for secret in ["SUN_MASTER_KEY", MASTER_KEY] {
        assert!(
            !seen.contains(secret),
            "the terminal shows {secret}: {seen}"
        );
    }
}

/// Keystroke echo latency and `cat` throughput through kernel and host on
/// loopback. `cargo test -p sun-api --test it terminal::measure -- --ignored --nocapture`.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "a measurement, not a check; run by hand"]
async fn measure_echo_latency_and_throughput() {
    let w = world().await;
    let owner = member_token(&w.k.store, "olive", Role::Owner).await;
    w.open_editor(&owner).await;
    let mut ws = w.connect(&owner, "").await.unwrap();
    opened(&mut ws).await;
    // A plain prompt and no line editing, so each key echoes one byte.
    type_line(&mut ws, "exec /bin/sh").await;
    type_line(&mut ws, "PS1='$ '; echo rea''dy").await;
    let mut screen = Screen::default();
    read_until(&mut ws, &mut screen, |s| s.output.contains("ready")).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    while let Ok(Some(_)) = tokio::time::timeout(Duration::from_millis(100), ws.next()).await {}

    let mut took = Vec::new();
    for _ in 0..300 {
        let sent = Instant::now();
        ws.send(Message::binary(b"a".to_vec())).await.unwrap();
        loop {
            if let Some(Ok(Message::Binary(_))) = ws.next().await {
                break;
            }
        }
        took.push(sent.elapsed());
    }
    took.sort();
    let at = |q: f64| took[((took.len() - 1) as f64 * q) as usize];
    println!(
        "echo latency over {} keys: p50 {:?}, p95 {:?}, p99 {:?}, max {:?}",
        took.len(),
        at(0.5),
        at(0.95),
        at(0.99),
        took[took.len() - 1]
    );
    ws.send(Message::binary(b"\x15".to_vec())).await.unwrap(); // ^U: drop the a's

    // 32 MiB of text lines.
    let root = w._dir.path().join("host/editor");
    let worktree = std::fs::read_dir(&root)
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    let worktree = std::fs::read_dir(worktree)
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    let line = format!("{}\n", "x".repeat(127));
    std::fs::write(
        worktree.join("big.txt"),
        line.repeat(32 * 1024 * 1024 / 128),
    )
    .unwrap();
    let mut screen = Screen::default();
    let started = Instant::now();
    type_line(&mut ws, "cat big.txt; echo do''ne").await;
    read_until(&mut ws, &mut screen, |s| s.output.ends_with("done\r\n$ ")).await;
    let secs = started.elapsed().as_secs_f64();
    let bytes = screen.output.len() as f64;
    println!(
        "cat: {:.1} MiB in {secs:.2} s = {:.1} MiB/s",
        bytes / 1048576.0,
        bytes / 1048576.0 / secs
    );
    assert!(screen.output.contains(&line.replace('\n', "\r\n")));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_host_older_than_terminals_is_refused_without_a_frame() {
    let k = kernel().await;
    let mut hello = crate::api::kernel_host_hello(&k);
    hello["protocol"] = json!("2.2");
    let mut host = crate::api::connect_host(&k.app, k.host, hello).await;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(axum::serve(listener, k.app.clone()).into_future());
    let repo = common::local_repo(&k, WorkspaceId::MAIN, "/srv/app").await;
    let w = World {
        base: format!("/api/v1/workspaces/{}/repos/{repo}", WorkspaceId::MAIN),
        addr,
        k,
        _dir: tempfile::tempdir().unwrap(),
    };
    let owner = member_token(&w.k.store, "olive", Role::Owner).await;
    let mut ws = w.connect(&owner, "").await.unwrap();
    let mut screen = Screen::default();
    read_until(&mut ws, &mut screen, |s| !s.controls.is_empty()).await;
    assert_eq!(screen.controls[0]["kind"], "error");
    let message = screen.controls[0]["message"].as_str().unwrap();
    assert!(message.contains("too old for the terminal"), "{message}");
    // The host never heard of it.
    let next = tokio::time::timeout(Duration::from_millis(300), host.next()).await;
    assert!(!matches!(next, Ok(Some(Ok(Message::Text(_))))), "{next:?}");
}
