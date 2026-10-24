//! Terminals under abuse: who may reattach a `pty_id` (revoked, another
//! member, repo or host, a foreign `Origin`), who may dial `/host/pty` for
//! one (a wrong key, an unknown id, another host's id), the per-member cap
//! with parked terminals, out-of-range resizes, and that nothing typed or
//! shown reaches the audit, the store or the logs.
#![allow(clippy::unwrap_used, clippy::expect_used)] // test helpers

use std::time::Duration;

use axum::http::StatusCode;
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use sun_core::model::{Host, Role};
use sun_core::{HostId, WorkspaceId};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::{self, Message};

use crate::common::{self, SECRET, kernel};
use crate::editor_grants::member_id;
use crate::terminal::{
    Screen, Socket, World, number_after, opened, read_until, refused, type_line, world_with,
};
use crate::workspaces::{call, call_as, member_token};

fn host_key(host: HostId) -> String {
    Sha256::digest(format!("{SECRET}:{host}").as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// Enrols another host (its key is [`host_key`]), attached to Main or not.
async fn enrol(store: &sun_store::Store, name: &str, in_main: bool) -> HostId {
    use sun_store::{HostRepo, WorkspaceItem, WorkspaceRepo};
    let id = HostId::generate();
    let mut tx = store.begin().await.unwrap();
    tx.insert_host(&Host {
        id,
        name: name.into(),
        capabilities: json!({}),
        enrolled_by: None,
    })
    .await
    .unwrap();
    if in_main {
        tx.link(WorkspaceId::MAIN, WorkspaceItem::Host(id))
            .await
            .unwrap();
    }
    tx.commit().await.unwrap();
    id
}

/// The terminal socket of `base` on `host` as `token`, with `query` after
/// `host_id` and an optional `Origin`.
async fn connect_to(
    addr: std::net::SocketAddr,
    base: &str,
    host: HostId,
    token: &str,
    query: &str,
    origin: Option<&str>,
) -> Result<Socket, tungstenite::Error> {
    let url = format!("ws://{addr}{base}/terminal?host_id={host}{query}");
    let mut request = url.into_client_request().unwrap();
    let bearer = format!("Bearer {token}").parse().unwrap();
    request.headers_mut().insert("authorization", bearer);
    if let Some(origin) = origin {
        request
            .headers_mut()
            .insert("origin", origin.parse().unwrap());
    }
    tokio_tungstenite::connect_async(request)
        .await
        .map(|(ws, _)| ws)
}

/// Types `line`, then reads until `marker=<digits>` shows; those digits.
async fn ask(ws: &mut Socket, line: &str, marker: &str) -> String {
    type_line(ws, line).await;
    let mut screen = Screen::default();
    read_until(ws, &mut screen, |s| {
        number_after(&s.output, marker).is_some()
    })
    .await;
    number_after(&screen.output, marker).unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn a_terminal_reattaches_only_for_its_member_repo_and_host_and_page() {
    let w = world_with(kernel().await, Duration::from_secs(30)).await;
    let olive = member_token(&w.k.store, "olive", Role::Owner).await;
    let otto = member_token(&w.k.store, "otto", Role::Owner).await;
    w.open_editor(&olive).await;
    w.open_editor(&otto).await;
    let other_repo = common::local_repo(&w.k, WorkspaceId::MAIN, "/srv/other").await;
    let other_base = format!(
        "/api/v1/workspaces/{}/repos/{other_repo}",
        WorkspaceId::MAIN
    );
    let other_host = enrol(&w.k.store, "other-box", true).await;

    let mut ws = w.connect(&olive, "").await.unwrap();
    let pty_id = opened(&mut ws).await;
    let shell = ask(&mut ws, "echo shell=$$", "shell").await;
    let again = format!("&pty_id={pty_id}");

    // A valid `pty_id`, live: anyone but its owner on its repo and host is
    // told it doesn't exist (and so can't replace the owner's socket).
    let (addr, base, host) = (w.addr, w.base.as_str(), w.k.host);
    let otto_try = connect_to(addr, base, host, &otto, &again, None).await;
    assert_eq!(refused(otto_try), StatusCode::NOT_FOUND, "another member");
    let repo_try = connect_to(addr, &other_base, host, &olive, &again, None).await;
    assert_eq!(refused(repo_try), StatusCode::NOT_FOUND, "another repo");
    let host_try = connect_to(addr, base, other_host, &olive, &again, None).await;
    assert_eq!(refused(host_try), StatusCode::NOT_FOUND, "another host");
    // A page on another origin is refused whatever it asks for; one on the
    // kernel's own origin is not.
    let evil = Some("https://evil.example");
    let foreign = connect_to(addr, base, host, &olive, &again, evil).await;
    assert_eq!(
        refused(foreign),
        StatusCode::FORBIDDEN,
        "foreign Origin, reattach"
    );
    let foreign = connect_to(addr, base, host, &olive, "", evil).await;
    assert_eq!(
        refused(foreign),
        StatusCode::FORBIDDEN,
        "foreign Origin, new"
    );
    let look_alike = Some(format!("http://{addr}.evil.example"));
    let foreign = connect_to(addr, base, host, &olive, "", look_alike.as_deref()).await;
    assert_eq!(refused(foreign), StatusCode::FORBIDDEN, "look-alike Origin");

    // None of that touched the owner's terminal: same shell, still live.
    assert_eq!(ask(&mut ws, "echo still=$$", "still").await, shell);

    // Parked, the same holds; then the owner's own page reattaches it.
    drop(ws);
    let otto_try = connect_to(addr, base, host, &otto, &again, None).await;
    assert_eq!(
        refused(otto_try),
        StatusCode::NOT_FOUND,
        "another member, parked"
    );
    let host_try = connect_to(addr, base, other_host, &olive, &again, None).await;
    assert_eq!(
        refused(host_try),
        StatusCode::NOT_FOUND,
        "another host, parked"
    );
    let own = Some(format!("http://{addr}"));
    let mut ws = connect_to(addr, base, host, &olive, &again, own.as_deref())
        .await
        .unwrap();
    assert_eq!(opened(&mut ws).await, pty_id);
    assert_eq!(ask(&mut ws, "echo back=$$", "back").await, shell);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_grant_revoked_mid_session_refuses_the_next_reattach() {
    let w = world_with(kernel().await, Duration::from_secs(30)).await;
    let mia = member_token(&w.k.store, "mia", Role::Member).await;
    let mia_id = member_id(&w.k.store, "mia").await;
    let grants = format!("/api/v1/workspaces/{}/editor/grants", WorkspaceId::MAIN);
    let app = &w.k.app;
    for capability in ["editor.read", "terminal"] {
        let grant =
            json!({"subject_type": "member", "subject_id": mia_id, "capability": capability});
        let (status, made) = call_as(app, common::TOKEN, "POST", &grants, Some(grant)).await;
        assert_eq!(status, StatusCode::CREATED, "{made}");
        if capability == "terminal" {
            w.open_editor(&mia).await;
            let mut ws = w.connect(&mia, "").await.unwrap();
            let pty_id = opened(&mut ws).await;
            ask(&mut ws, "echo before=$$", "before").await;

            let revoke = format!("{grants}/{}", made["id"].as_str().unwrap());
            let (status, _) = call_as(app, common::TOKEN, "DELETE", &revoke, None).await;
            assert_eq!(status, StatusCode::NO_CONTENT);
            // While her socket is still live, and after it closed: 403, and
            // a new terminal too.
            let again = format!("&pty_id={pty_id}");
            assert_eq!(
                refused(w.connect(&mia, &again).await),
                StatusCode::FORBIDDEN
            );
            drop(ws);
            assert_eq!(
                refused(w.connect(&mia, &again).await),
                StatusCode::FORBIDDEN
            );
            assert_eq!(refused(w.connect(&mia, "").await), StatusCode::FORBIDDEN);
        }
    }
}

/// A kernel on TCP whose host is a fake on the control socket, so a test
/// holds a `pty.open` and dials `/host/pty` itself.
struct FakeHost {
    w: World,
    control: Socket,
}

async fn fake_host() -> FakeHost {
    let k = kernel().await;
    let mut hello = crate::api::kernel_host_hello(&k);
    hello["protocol"] = json!("2.3");
    let control = crate::api::connect_host(&k.app, k.host, hello).await;
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
    FakeHost { w, control }
}

/// Dials `/host/pty` with this hello, then sends `bytes` as if PTY output.
async fn dial(addr: std::net::SocketAddr, hello: Value, bytes: &[u8]) -> Socket {
    let url = format!("ws://{addr}/host/pty");
    let (mut ws, _) = tokio_tungstenite::connect_async(url).await.unwrap();
    ws.send(Message::text(hello.to_string())).await.unwrap();
    let _ = ws.send(Message::binary(bytes.to_vec())).await;
    ws
}

/// The kernel drops the socket without sending it anything (the host then
/// kills that PTY).
async fn dropped(ws: &mut Socket, who: &str) {
    let next = tokio::time::timeout(Duration::from_secs(5), ws.next())
        .await
        .unwrap_or_else(|_| panic!("{who}: the kernel kept the socket open"));
    assert!(
        matches!(next, None | Some(Err(_)) | Some(Ok(Message::Close(_)))),
        "{who}: got {next:?}"
    );
}

/// Nothing arrives on `ws` for a moment.
async fn quiet(ws: &mut Socket, who: &str) {
    let next = tokio::time::timeout(Duration::from_millis(300), ws.next()).await;
    assert!(next.is_err(), "{who}: {next:?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn only_the_asked_host_with_its_key_carries_a_terminal() {
    let FakeHost { w, mut control } = fake_host().await;
    let owner = member_token(&w.k.store, "olive", Role::Owner).await;
    let impostor = enrol(&w.k.store, "impostor", false).await;

    let mut browser = w.connect(&owner, "").await.unwrap();
    let frame = crate::api::recv(&mut control).await.unwrap();
    assert_eq!(frame["type"], "pty.open", "{frame}");
    let pty_id = frame["pty_id"].as_str().unwrap().to_owned();
    let done = json!({ "type": "reply", "id": frame["id"], "body": { "kind": "done" } });
    crate::api::send(&mut control, done).await;

    // The asked host with a wrong key; a real host with an unknown id;
    // another real host claiming this id. Each is dropped unanswered, and
    // the browser hears nothing of them.
    let hello = |host: HostId, key: String, pty: &str| json!({"host_id": host, "host_key": key, "pty_id": pty});
    let cases = [
        ("wrong key", hello(w.k.host, host_key(impostor), &pty_id)),
        ("garbage key", hello(w.k.host, "0".repeat(64), &pty_id)),
        (
            "unknown pty_id",
            hello(w.k.host, host_key(w.k.host), "01NOSUCHTERMINAL"),
        ),
        (
            "another host's pty_id",
            hello(impostor, host_key(impostor), &pty_id),
        ),
    ];
    for (who, hello) in cases {
        let mut ws = dial(w.addr, hello, b"IMPOSTOR").await;
        dropped(&mut ws, who).await;
        quiet(&mut browser, who).await;
    }
    // Not JSON, and no hello at all, are dropped too.
    let (mut ws, _) = tokio_tungstenite::connect_async(format!("ws://{}/host/pty", w.addr))
        .await
        .unwrap();
    ws.send(Message::binary(b"IMPOSTOR".to_vec()))
        .await
        .unwrap();
    dropped(&mut ws, "binary first").await;

    // The slot was not used up: the asked host, with its key, carries it.
    let real = hello(w.k.host, host_key(w.k.host), &pty_id);
    let mut host = dial(w.addr, real, b"").await;
    assert_eq!(opened(&mut browser).await, pty_id);
    host.send(Message::binary(b"LEGIT".to_vec())).await.unwrap();
    let mut screen = Screen::default();
    read_until(&mut browser, &mut screen, |s| s.output.contains("LEGIT")).await;
    assert!(!screen.output.contains("IMPOSTOR"), "{:?}", screen.output);
    // And only once: a second socket for the same id is dropped.
    let twice = hello(w.k.host, host_key(w.k.host), &pty_id);
    let mut second = dial(w.addr, twice, b"IMPOSTOR").await;
    dropped(&mut second, "second dial of a live id").await;
    host.send(Message::binary(b"STILL".to_vec())).await.unwrap();
    read_until(&mut browser, &mut screen, |s| s.output.contains("STILL")).await;
    assert!(!screen.output.contains("IMPOSTOR"), "{:?}", screen.output);
}

#[tokio::test(flavor = "multi_thread")]
async fn parked_terminals_count_toward_the_cap_and_bad_resizes_are_dropped() {
    let w = world_with(kernel().await, Duration::from_secs(30)).await;
    let owner = member_token(&w.k.store, "olive", Role::Owner).await;
    w.open_editor(&owner).await;

    // Out-of-range or malformed resizes are dropped; the session lives on
    // at its size, and a valid one still applies.
    let mut ws = w.connect(&owner, "&cols=80&rows=24").await.unwrap();
    let first = opened(&mut ws).await;
    let shell = ask(&mut ws, "echo shell=$$", "shell").await;
    for bad in [
        json!({"kind": "resize", "cols": 0, "rows": 24}),
        json!({"kind": "resize", "cols": 1001, "rows": 24}),
        json!({"kind": "resize", "cols": 80, "rows": 0}),
        json!({"kind": "resize", "cols": 80, "rows": 1001}),
        json!({"kind": "resize", "cols": 70000, "rows": 24}),
        json!({"kind": "resize", "cols": -1, "rows": 24}),
        json!({"kind": "resize"}),
        json!({"kind": "exit", "code": 0}),
    ] {
        ws.send(Message::text(bad.to_string())).await.unwrap();
    }
    ws.send(Message::text("not json")).await.unwrap();
    type_line(&mut ws, "stty size; echo sized=$$").await;
    let mut screen = Screen::default();
    read_until(&mut ws, &mut screen, |s| {
        number_after(&s.output, "sized").is_some()
    })
    .await;
    assert!(screen.output.contains("24 80"), "{:?}", screen.output);
    assert_eq!(number_after(&screen.output, "sized").unwrap(), shell);
    let edge = json!({"kind": "resize", "cols": 1000, "rows": 1}).to_string();
    ws.send(Message::text(edge)).await.unwrap();
    type_line(&mut ws, "stty size").await;
    let mut screen = Screen::default();
    read_until(&mut ws, &mut screen, |s| s.output.contains("1 1000")).await;

    // Five open, three of them parked (their tabs closed within the grace):
    // a sixth is still refused.
    let mut open = vec![(first, ws)];
    for _ in 0..4 {
        let mut ws = w.connect(&owner, "").await.unwrap();
        let id = opened(&mut ws).await;
        open.push((id, ws));
    }
    let parked: Vec<String> = open
        .drain(..3)
        .map(|(id, ws)| {
            drop(ws);
            id
        })
        .collect();
    let mut sixth = w.connect(&owner, "").await.unwrap();
    let mut screen = Screen::default();
    read_until(&mut sixth, &mut screen, |s| !s.controls.is_empty()).await;
    assert_eq!(screen.controls[0]["kind"], "error", "{:?}", screen.controls);
    assert!(
        screen.controls[0]["message"]
            .as_str()
            .unwrap()
            .contains("at most 5"),
        "{:?}",
        screen.controls
    );
    // The parked ones were kept, not reaped to make room.
    for id in &parked {
        let mut ws = w.connect(&owner, &format!("&pty_id={id}")).await.unwrap();
        assert_eq!(&opened(&mut ws).await, id);
    }
}

/// Typed and echoed text: in neither the audit, the kernel's files nor
/// anything the kernel or host printed. Runs itself in a child process to
/// capture that output (`SUN_TERMINAL_LEAK_CHILD`).
#[tokio::test(flavor = "multi_thread")]
async fn nothing_typed_or_shown_reaches_the_audit_the_store_or_the_logs() {
    const SENTINEL: &str = "SUNLEAKCANARY7f3a91";
    if std::env::var_os("SUN_TERMINAL_LEAK_CHILD").is_none() {
        let out = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "terminal_abuse::nothing_typed_or_shown_reaches_the_audit_the_store_or_the_logs",
                "--exact",
                "--nocapture",
                "--test-threads=1",
            ])
            .env("SUN_TERMINAL_LEAK_CHILD", "1")
            .output()
            .unwrap();
        let (stdout, stderr) = (
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr),
        );
        assert!(out.status.success(), "child failed:\n{stdout}\n{stderr}");
        assert!(
            stdout.contains("1 passed"),
            "the child ran nothing:\n{stdout}"
        );
        assert!(stdout.contains("leak-child: done"), "{stdout}");
        assert!(!stdout.contains(SENTINEL), "in stdout:\n{stdout}");
        assert!(!stderr.contains(SENTINEL), "in stderr:\n{stderr}");
        return;
    }

    let dir = tempfile::tempdir().unwrap();
    let store_dir = dir.path().to_path_buf();
    let w = world_with(common::kernel_in(dir).await, Duration::from_secs(1)).await;
    let owner = member_token(&w.k.store, "olive", Role::Owner).await;
    w.open_editor(&owner).await;
    let mut ws = w.connect(&owner, "").await.unwrap();
    opened(&mut ws).await;
    // Typed in pieces so the echo and the output both carry it whole.
    type_line(&mut ws, &format!("echo {SENTINEL}; echo sent=$$")).await;
    let mut screen = Screen::default();
    read_until(&mut ws, &mut screen, |s| {
        number_after(&s.output, "sent").is_some()
    })
    .await;
    let seen = screen.output.matches(SENTINEL).count();
    type_line(&mut ws, "exit").await;
    read_until(&mut ws, &mut screen, |s| {
        s.controls.iter().any(|c| c["kind"] == "exit")
    })
    .await;
    drop(ws);

    // The audit has the terminal's open and close, and not the text.
    let mut audit = Value::Null;
    for _ in 0..100 {
        let (status, body) = call(&w.k.app, "GET", "/api/v1/audit", None).await;
        assert_eq!(status, StatusCode::OK);
        if body.to_string().contains("terminal.close") {
            audit = body;
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let terminal: Vec<&Value> = audit
        .as_array()
        .expect("terminal.close never audited")
        .iter()
        .filter(|e| e["action"] == "terminal")
        .collect();
    assert!(terminal.len() >= 2, "{terminal:?}");
    let leaked_audit = audit.to_string().contains(SENTINEL);
    // Every byte the kernel keeps on disk (the store and its WAL).
    let mut leaked_files = Vec::new();
    let mut stack = vec![store_dir];
    while let Some(next) = stack.pop() {
        for item in std::fs::read_dir(&next).unwrap() {
            let path = item.unwrap().path();
            if path.is_dir() {
                stack.push(path);
            } else if let Ok(bytes) = std::fs::read(&path) {
                let hit = bytes
                    .windows(SENTINEL.len())
                    .any(|w| w == SENTINEL.as_bytes());
                if hit {
                    leaked_files.push(path.display().to_string());
                }
            }
        }
    }
    // Asserted without printing the screen, which holds the sentinel.
    assert!(
        seen >= 2,
        "the sentinel never went through the terminal ({seen})"
    );
    assert!(!leaked_audit, "the sentinel is in the audit");
    assert!(
        leaked_files.is_empty(),
        "the sentinel is on disk: {leaked_files:?}"
    );
    println!("leak-child: done");
}
