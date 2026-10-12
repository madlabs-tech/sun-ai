//! The host against an in-test kernel stub (the real `/host` endpoint lives
//! in `sun-api`) and a fake runtime adapter that runs `sleep` as its "agent".
#![allow(clippy::unwrap_used)] // test helpers

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use futures_util::{SinkExt, StreamExt};
use sun_core::host_protocol::{
    ExecRequest, HostFrame, KernelFrame, ReplyBody, SessionStats, Worktree,
};
use sun_core::{
    BoxStream, ExecEnvSpec, HostCtx, HostId, Readiness, Result, Role, RunId, RuntimeAdapter,
    SessionHandle, SessionId, SessionSpec, Steer, StepKey, StopMode, SunEvent, TaskId, Transcript,
    VmSpec,
};
use sun_host::{Credentials, Host, HostConfig, enroll};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::Message;

const WAIT: Duration = Duration::from_secs(10);

// ---------- fake runtime ----------

#[derive(Default)]
struct Fake {
    sessions: Mutex<HashMap<SessionId, FakeSession>>,
    specs: Mutex<Vec<SessionSpec>>,
}

struct FakeSession {
    child: tokio::process::Child,
    tx: Option<mpsc::UnboundedSender<SunEvent>>,
    rx: Option<mpsc::UnboundedReceiver<SunEvent>>,
}

#[async_trait]
impl RuntimeAdapter for Fake {
    fn id(&self) -> &str {
        "fake"
    }

    async fn readiness(&self, _: &HostCtx) -> Readiness {
        Readiness {
            installed: true,
            version: Some("1.0".into()),
            logged_in: true,
            detail: None,
        }
    }

    async fn start(&self, spec: SessionSpec) -> Result<SessionHandle> {
        let child = tokio::process::Command::new("sleep")
            .arg("30")
            .current_dir(&spec.worktree)
            .process_group(0)
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let (tx, rx) = mpsc::unbounded_channel();
        for text in ["hello", "working"] {
            tx.send(SunEvent::Text {
                role: Role::Assistant,
                text: text.into(),
            })
            .unwrap();
        }
        let handle = SessionHandle {
            session_id: spec.session_id,
            native_id: None,
            pid: child.id(),
        };
        self.sessions.lock().unwrap().insert(
            spec.session_id,
            FakeSession {
                child,
                tx: Some(tx),
                rx: Some(rx),
            },
        );
        self.specs.lock().unwrap().push(spec);
        Ok(handle)
    }

    async fn send(&self, session: &SessionHandle, msg: Steer) -> Result<()> {
        let sessions = self.sessions.lock().unwrap();
        let tx = sessions[&session.session_id].tx.as_ref().unwrap();
        tx.send(SunEvent::Text {
            role: Role::User,
            text: msg.text,
        })
        .unwrap();
        Ok(())
    }

    async fn stop(&self, session: &SessionHandle, _: StopMode) -> Result<()> {
        let mut sessions = self.sessions.lock().unwrap();
        let s = sessions.get_mut(&session.session_id).unwrap();
        let _ = s.child.start_kill();
        if let Some(tx) = s.tx.take() {
            tx.send(SunEvent::Exited { code: None }).unwrap();
        }
        Ok(())
    }

    fn events(&self, session: &SessionHandle) -> BoxStream<'static, SunEvent> {
        let rx = self
            .sessions
            .lock()
            .unwrap()
            .get_mut(&session.session_id)
            .unwrap()
            .rx
            .take()
            .unwrap();
        Box::pin(Rx(rx))
    }

    async fn export(&self, _: &SessionHandle) -> Result<Transcript> {
        unimplemented!()
    }

    fn context_window(&self, _: &str) -> Option<u32> {
        None
    }
}

struct Rx(mpsc::UnboundedReceiver<SunEvent>);

impl futures_core::Stream for Rx {
    type Item = SunEvent;
    fn poll_next(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<SunEvent>> {
        self.0.poll_recv(cx)
    }
}

// ---------- kernel stub ----------

struct Kernel {
    ws: WebSocketStream<TcpStream>,
    next_id: u64,
}

impl Kernel {
    async fn accept(listener: &TcpListener) -> Self {
        let (tcp, _) = tokio::time::timeout(WAIT, listener.accept())
            .await
            .unwrap()
            .unwrap();
        Self {
            ws: tokio_tungstenite::accept_async(tcp).await.unwrap(),
            next_id: 0,
        }
    }

    async fn send(&mut self, frame: &KernelFrame) {
        let text = serde_json::to_string(frame).unwrap();
        self.ws.send(Message::text(text)).await.unwrap();
    }

    /// Next frame, skipping `stats` unless asked for.
    async fn recv(&mut self, want_stats: bool) -> HostFrame {
        loop {
            let msg = tokio::time::timeout(WAIT, self.ws.next())
                .await
                .unwrap()
                .unwrap()
                .unwrap();
            let Message::Text(text) = msg else { continue };
            let frame: HostFrame = serde_json::from_str(&text).unwrap();
            if want_stats || !matches!(frame, HostFrame::Stats(_)) {
                return frame;
            }
        }
    }

    async fn request(&mut self, make: impl FnOnce(u64) -> KernelFrame) -> ReplyBody {
        self.next_id += 1;
        let id = self.next_id;
        self.send(&make(id)).await;
        match self.recv(false).await {
            HostFrame::Reply { id: got, body } if got == id => body,
            other => panic!("expected reply {id}, got {other:?}"),
        }
    }

    async fn hello(&mut self) -> BTreeMap<SessionId, u64> {
        match self.recv(false).await {
            HostFrame::Hello {
                host_key,
                capabilities,
                last_ack,
                ..
            } => {
                assert_eq!(host_key, "key-1");
                assert_eq!(capabilities.runtimes[0].adapter, "fake");
                last_ack
            }
            other => panic!("expected hello, got {other:?}"),
        }
    }

    async fn event(&mut self) -> (u64, SunEvent) {
        match self.recv(false).await {
            HostFrame::Event(e) => (e.seq, e.event),
            other => panic!("expected event, got {other:?}"),
        }
    }
}

/// Writes an executable script.
///
/// ponytail: rust-lang/rust#114554. A fork in another test thread can inherit
/// a write fd we hold on the script until its exec closes it (O_CLOEXEC), and
/// an exec in that window fails with "Text file busy". A `sh` child writes the
/// file, so this process never holds that fd (a `--version` warm-up would run
/// the fake and add lines to the argv log the test asserts on). Drop this once
/// std closes the race.
fn script(path: &Path, text: &str) {
    use std::io::Write;
    let mut sh = std::process::Command::new("sh")
        .args(["-c", "cat > \"$0\" && chmod 755 \"$0\""])
        .arg(path)
        .stdin(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    sh.stdin.take().unwrap().write_all(text.as_bytes()).unwrap();
    assert!(sh.wait().unwrap().success(), "{}", path.display());
}

fn temp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("sun-host-it-{name}-{}", SessionId::generate()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

async fn listen() -> (TcpListener, String) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("ws://{}/host", listener.local_addr().unwrap());
    (listener, url)
}

/// Running and not a zombie (the fake never reaps its child).
fn pid_alive(pid: u32) -> bool {
    let out = std::process::Command::new("ps")
        .args(["-o", "stat=", "-p", &pid.to_string()])
        .output()
        .unwrap();
    let stat = String::from_utf8_lossy(&out.stdout);
    !stat.trim().is_empty() && !stat.trim_start().starts_with('Z')
}

// ---------- tests ----------

#[tokio::test]
async fn enrolment_trades_the_token_for_a_saved_host_key() {
    let (listener, url) = listen().await;
    let data_dir = temp_dir("enroll");
    let host_id = HostId::generate();
    let kernel = tokio::spawn(async move {
        let mut kernel = Kernel::accept(&listener).await;
        let HostFrame::Enroll { token, name } = kernel.recv(false).await else {
            panic!("expected enroll")
        };
        assert_eq!((token.as_str(), name.as_str()), ("one-time", "mac-mini"));
        kernel
            .send(&KernelFrame::Enrolled {
                host_id,
                host_key: "key-1".into(),
            })
            .await;
    });
    let credentials = enroll(&url, "one-time", "mac-mini", &data_dir)
        .await
        .unwrap();
    kernel.await.unwrap();
    assert_eq!(credentials.host_id, host_id);
    assert_eq!(Credentials::load(&data_dir).unwrap(), credentials);
    let mode = std::os::unix::fs::PermissionsExt::mode(
        &std::fs::metadata(data_dir.join("host.json"))
            .unwrap()
            .permissions(),
    );
    assert_eq!(mode & 0o777, 0o600);
}

#[tokio::test(flavor = "multi_thread")]
async fn runs_a_session_and_never_loses_an_event_across_reconnects() {
    let (listener, url) = listen().await;
    let data_dir = temp_dir("session");
    let mut config = HostConfig::new(&data_dir);
    config.stats_every = Duration::from_millis(100);
    config.backoff_min = Duration::from_millis(20);
    config.backoff_max = Duration::from_millis(100);
    let fake = Arc::new(Fake::default());
    let host = Host::open(config, vec![fake.clone()]).unwrap();
    let credentials = Credentials {
        kernel_url: url,
        host_id: HostId::generate(),
        host_key: "key-1".into(),
    };
    let running = tokio::spawn(Arc::clone(&host).run(credentials));

    let mut kernel = Kernel::accept(&listener).await;
    assert!(kernel.hello().await.is_empty());

    // Worktree: a task folder, idempotent.
    let task_id = TaskId::generate();
    let prepare = |id| KernelFrame::WorktreePrepare {
        id,
        worktree: Worktree::Folder { task_id },
    };
    let ReplyBody::Worktree { path } = kernel.request(prepare).await else {
        panic!()
    };
    assert!(path.ends_with(task_id.to_string()) && path.is_dir());
    assert_eq!(
        kernel.request(prepare).await,
        ReplyBody::Worktree { path: path.clone() }
    );

    // A session may only run in a prepared worktree.
    let spec = |worktree: &Path| SessionSpec {
        skills: None,
        session_id: SessionId::generate(),
        run_id: RunId::generate(),
        step_key: StepKey::new("implement").unwrap(),
        worktree: worktree.into(),
        envelope: "do it".into(),
        envelope_parts: None,
        model: None,
        mcp_config: None,
        env: BTreeMap::new(),
        exec_env: Default::default(),
        resume: None,
        permission_mode: None,
        byo_key: None,
    };
    let outside = spec(&std::env::temp_dir());
    let body = kernel
        .request(|id| KernelFrame::SessionStart {
            id,
            adapter: "fake".into(),
            spec: Box::new(outside),
        })
        .await;
    assert!(
        matches!(body, ReplyBody::Error { ref code, .. } if code == "invalid_input"),
        "{body:?}"
    );

    let spec = spec(&path);
    let session_id = spec.session_id;
    let ReplyBody::Session { handle } = kernel
        .request(|id| KernelFrame::SessionStart {
            id,
            adapter: "fake".into(),
            spec: Box::new(spec),
        })
        .await
    else {
        panic!()
    };
    let pid = handle.pid.unwrap();
    let tmpdir = fake.specs.lock().unwrap()[0].env["TMPDIR"].clone();
    assert!(Path::new(&tmpdir).is_dir(), "session has its own TMPDIR");

    assert_eq!(kernel.event().await.0, 1);
    assert_eq!(kernel.event().await.0, 2);
    let steer = kernel
        .request(|id| KernelFrame::SessionSend {
            id,
            session_id,
            text: "also add tests".into(),
        })
        .await;
    assert_eq!(steer, ReplyBody::Done);
    assert_eq!(kernel.event().await.0, 3);

    // Stats for the session's process tree.
    loop {
        if let HostFrame::Stats(SessionStats {
            session_id: s,
            rss_bytes,
            ..
        }) = kernel.recv(true).await
        {
            assert_eq!(s, session_id);
            assert!(rss_bytes > 0);
            break;
        }
    }
    let alive = kernel
        .request(|id| KernelFrame::Probe { id, session_id })
        .await;
    assert_eq!(alive, ReplyBody::Alive { alive: true });

    // Kernel stores 1..=2 only, then drops the connection.
    kernel.send(&KernelFrame::Ack { session_id, seq: 2 }).await;
    drop(kernel);

    // Reconnect: hello reports the ack, the host resends seq 3 only.
    let mut kernel = Kernel::accept(&listener).await;
    assert_eq!(kernel.hello().await, BTreeMap::from([(session_id, 2)]));
    assert_eq!(kernel.event().await.0, 3);
    kernel.send(&KernelFrame::Ack { session_id, seq: 3 }).await;

    // Kill: the process tree dies and the session ends with `exited`.
    let stop = kernel
        .request(|id| KernelFrame::SessionStop {
            id,
            session_id,
            how: StopMode::Kill,
        })
        .await;
    assert_eq!(stop, ReplyBody::Done);
    assert_eq!(kernel.event().await, (4, SunEvent::Exited { code: None }));
    let alive = kernel
        .request(|id| KernelFrame::Probe { id, session_id })
        .await;
    assert_eq!(alive, ReplyBody::Alive { alive: false });
    assert!(!pid_alive(pid), "agent process killed");
    assert!(!Path::new(&tmpdir).exists(), "TMPDIR removed after exit");

    // Unacked `exited` survives another reconnect; acking it empties the buffer.
    drop(kernel);
    let mut kernel = Kernel::accept(&listener).await;
    assert_eq!(kernel.hello().await, BTreeMap::from([(session_id, 3)]));
    assert_eq!(kernel.event().await.0, 4);
    kernel.send(&KernelFrame::Ack { session_id, seq: 4 }).await;
    let stop = kernel
        .request(|id| KernelFrame::SessionStop {
            id,
            session_id,
            how: StopMode::Graceful,
        })
        .await;
    assert_eq!(stop, ReplyBody::Done, "stop is idempotent");
    assert_eq!(
        std::fs::read_dir(data_dir.join("buffer")).unwrap().count(),
        0
    );

    // exec: code + redacted output, only inside a worktree, with a timeout.
    let exec = |cwd: &Path, command: &str, timeout_s| ExecRequest {
        cwd: cwd.into(),
        command: command.into(),
        env: BTreeMap::from([("GATE_TOKEN".into(), "s3cr3t-value-123".into())]),
        timeout_s,
    };
    let request = exec(&path, "pwd; echo $GATE_TOKEN; echo oops >&2; exit 3", 10);
    let ReplyBody::Exec { output } = kernel.request(|id| KernelFrame::Exec { id, request }).await
    else {
        panic!()
    };
    assert_eq!(output.code, Some(3));
    assert!(
        output.output.contains(&path.display().to_string()),
        "{}",
        output.output
    );
    assert!(output.output.contains("oops") && !output.output.contains("s3cr3t-value-123"));
    let request = exec(&path, "sleep 30", 1);
    let ReplyBody::Exec { output } = kernel.request(|id| KernelFrame::Exec { id, request }).await
    else {
        panic!()
    };
    assert!(output.timed_out && output.code.is_none());
    let request = exec(Path::new("/"), "true", 1);
    let body = kernel.request(|id| KernelFrame::Exec { id, request }).await;
    assert!(matches!(body, ReplyBody::Error { ref code, .. } if code == "invalid_input"));

    running.abort();
}

#[tokio::test]
async fn repo_worktree_is_a_git_worktree_off_a_cached_bare_clone() {
    let root = temp_dir("repo");
    let origin = root.join("origin");
    let git = |dir: &Path, args: &[&str]| {
        let out = std::process::Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(["-c", "user.name=t", "-c", "user.email=t@t"])
            .args(args)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).trim().to_owned()
    };
    std::fs::create_dir_all(&origin).unwrap();
    git(&origin, &["init", "-q", "-b", "main"]);
    std::fs::write(origin.join("README.md"), "hi").unwrap();
    git(&origin, &["add", "."]);
    git(&origin, &["commit", "-q", "-m", "init"]);

    let (listener, url) = listen().await;
    let host = Host::open(HostConfig::new(root.join("data")), vec![]).unwrap();
    let running = tokio::spawn(Arc::clone(&host).run(Credentials {
        kernel_url: url,
        host_id: HostId::generate(),
        host_key: "key-1".into(),
    }));
    let mut kernel = Kernel::accept(&listener).await;
    kernel.recv(false).await; // hello

    let branch = |task_id: TaskId| format!("sun/SUN-7-{task_id}");
    let repo = |task_id| Worktree::Repo {
        task_id,
        url: origin.display().to_string(),
        base: None,
        branch: branch(task_id),
    };
    let task_id = TaskId::generate();
    let ReplyBody::Worktree { path } = kernel
        .request(|id| KernelFrame::WorktreePrepare {
            id,
            worktree: repo(task_id),
        })
        .await
    else {
        panic!()
    };
    assert_eq!(
        git(&path, &["rev-parse", "--abbrev-ref", "HEAD"]),
        branch(task_id)
    );
    assert!(path.join("README.md").is_file());
    git(&path, &["check-ignore", "-q", ".sun/context.md"]); // .sun/ never committed

    // The worktree dir is lost: the next prepare checks the branch out again.
    std::fs::write(path.join("work.txt"), "x").unwrap();
    git(&path, &["add", "."]);
    git(&path, &["commit", "-q", "-m", "work"]);
    std::fs::remove_dir_all(&path).unwrap();
    let again = kernel
        .request(|id| KernelFrame::WorktreePrepare {
            id,
            worktree: repo(task_id),
        })
        .await;
    assert_eq!(again, ReplyBody::Worktree { path: path.clone() });
    assert!(path.join("work.txt").is_file(), "branch work kept");

    // A second task of the same repo reuses the cached clone.
    let other = kernel
        .request(|id| KernelFrame::WorktreePrepare {
            id,
            worktree: repo(TaskId::generate()),
        })
        .await;
    assert!(matches!(other, ReplyBody::Worktree { .. }), "{other:?}");
    assert_eq!(
        std::fs::read_dir(root.join("data/repos")).unwrap().count(),
        1
    );

    // A run moved here mid-step (marketplace failover) pushed its branch
    // after the clone was cached: its worktree continues from that branch.
    let moved = TaskId::generate();
    git(&origin, &["checkout", "-q", "-b", &branch(moved)]);
    std::fs::write(origin.join("checkpoint.txt"), "wip").unwrap();
    git(&origin, &["add", "."]);
    git(&origin, &["commit", "-q", "-m", "wip(sun): handoff from a"]);
    git(&origin, &["checkout", "-q", "main"]);
    let ReplyBody::Worktree { path } = kernel
        .request(|id| KernelFrame::WorktreePrepare {
            id,
            worktree: repo(moved),
        })
        .await
    else {
        panic!()
    };
    assert!(path.join("checkpoint.txt").is_file(), "the pushed branch");

    let bad = kernel
        .request(|id| KernelFrame::WorktreePrepare {
            id,
            worktree: Worktree::Repo {
                task_id,
                url: "ext::sh -c id".into(),
                base: None,
                branch: branch(task_id),
            },
        })
        .await;
    assert!(matches!(bad, ReplyBody::Error { ref code, .. } if code == "invalid_input"));
    // The branch comes from the kernel and is checked like any ref.
    let bad = kernel
        .request(|id| KernelFrame::WorktreePrepare {
            id,
            worktree: Worktree::Repo {
                task_id: TaskId::generate(),
                url: origin.display().to_string(),
                base: None,
                branch: "--force".into(),
            },
        })
        .await;
    assert!(matches!(bad, ReplyBody::Error { ref code, .. } if code == "invalid_input"));
    running.abort();
}

/// A `vm` session's env outlives the session so the step's gates run in it:
/// the next session in the same worktree releases it, otherwise it is
/// deleted after `env_linger` unused. A fake `limactl` logs its argv and runs
/// `shell` commands on the host.
#[tokio::test(flavor = "multi_thread")]
async fn vm_env_lingers_for_gates_then_is_deleted() {
    let (listener, url) = listen().await;
    let data_dir = temp_dir("vm");
    let log = data_dir.join("limactl.log");
    let limactl = data_dir.join("fake-limactl");
    script(
        &limactl,
        &format!(
            "#!/bin/sh\necho \"$*\" >> '{}'\n[ \"$1\" = shell ] || exit 0\nshift 2\n\
             cd \"$1\"; shift 2; exec \"$@\"\n",
            log.display()
        ),
    );
    let mut config = HostConfig::new(&data_dir);
    config.vm_cli = limactl;
    // Each gate must reach the host within the linger after its session
    // ended; 300 ms was too tight under a loaded `cargo test --workspace`.
    config.env_linger = Duration::from_secs(2);
    let host = Host::open(config, vec![Arc::new(Fake::default())]).unwrap();
    let credentials = Credentials {
        kernel_url: url,
        host_id: HostId::generate(),
        host_key: "key-1".into(),
    };
    let running = tokio::spawn(Arc::clone(&host).run(credentials));
    let mut kernel = Kernel::accept(&listener).await;
    kernel.hello().await;
    let task_id = TaskId::generate();
    let ReplyBody::Worktree { path } = kernel
        .request(|id| KernelFrame::WorktreePrepare {
            id,
            worktree: Worktree::Folder { task_id },
        })
        .await
    else {
        panic!()
    };

    let first = vm_step(&mut kernel, &path).await;
    let second = vm_step(&mut kernel, &path).await;
    // Wait for the reaper (its `delete`, the 8th call, and the TMPDIR
    // removal after it) instead of a fixed sleep.
    let tmp = data_dir.join("tmp");
    let log = tokio::time::timeout(WAIT, async {
        loop {
            // `--version` lines are the exec env probe of each `hello`.
            let text = std::fs::read_to_string(&log).unwrap();
            let text: String = text
                .lines()
                .filter(|l| *l != "--version")
                .map(|l| format!("{l}\n"))
                .collect();
            if text.lines().count() >= 8 && std::fs::read_dir(&tmp).unwrap().count() == 0 {
                return text;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .unwrap();
    let verbs: Vec<String> = log
        .lines()
        .map(|l| {
            let words: Vec<&str> = l.split(' ').collect();
            let name = words.iter().find(|w| w.starts_with("sun-")).unwrap();
            format!("{} {name}", words[0])
        })
        .collect();
    let want = [
        format!("delete {first}"),
        format!("start {first}"),
        format!("shell {first}"),  // the gate ran in the ended session's VM
        format!("delete {first}"), // released by the next session
        format!("delete {second}"),
        format!("start {second}"),
        format!("shell {second}"),
        format!("delete {second}"), // reaped after the linger
    ];
    assert_eq!(verbs, want, "{log}");
    assert_eq!(std::fs::read_dir(data_dir.join("tmp")).unwrap().count(), 0);
    running.abort();
}

/// Starts a `vm` session in `worktree`, ends it and runs a gate there;
/// returns the VM name.
async fn vm_step(kernel: &mut Kernel, worktree: &Path) -> String {
    let spec = SessionSpec {
        skills: None,
        session_id: SessionId::generate(),
        run_id: RunId::generate(),
        step_key: StepKey::new("implement").unwrap(),
        worktree: worktree.into(),
        envelope: "do it".into(),
        envelope_parts: None,
        model: None,
        mcp_config: None,
        env: BTreeMap::new(),
        exec_env: ExecEnvSpec::Vm(VmSpec {
            template: "default".into(),
            memory_mb: None,
            cpus: None,
            network: None,
            keep: false,
        }),
        resume: None,
        permission_mode: None,
        byo_key: None,
    };
    let session_id = spec.session_id;
    // Events and replies interleave: read past events, noting `exited`.
    let mut exited = false;
    let mut request = async |kernel: &mut Kernel, frame: KernelFrame| {
        kernel.send(&frame).await;
        loop {
            match kernel.recv(false).await {
                HostFrame::Reply { id: 0, body } => return body,
                HostFrame::Event(e) => exited |= e.event == SunEvent::Exited { code: None },
                other => panic!("{other:?}"),
            }
        }
    };
    let start = KernelFrame::SessionStart {
        id: 0,
        adapter: "fake".into(),
        spec: Box::new(spec),
    };
    let body = request(kernel, start).await;
    assert!(matches!(body, ReplyBody::Session { .. }), "{body:?}");
    let stop = KernelFrame::SessionStop {
        id: 0,
        session_id,
        how: StopMode::Kill,
    };
    assert_eq!(request(kernel, stop).await, ReplyBody::Done);
    while !exited {
        exited = kernel.event().await.1 == SunEvent::Exited { code: None };
    }
    let request = ExecRequest {
        cwd: worktree.into(),
        command: "echo gate-ran".into(),
        env: BTreeMap::new(),
        timeout_s: 10,
    };
    let ReplyBody::Exec { output } = kernel.request(|id| KernelFrame::Exec { id, request }).await
    else {
        panic!()
    };
    assert_eq!(output.output, "gate-ran\n");
    format!("sun-{}", session_id.to_string().to_lowercase())
}
